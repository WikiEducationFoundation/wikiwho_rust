//! `rev_content` endpoints (API.md §1-6).
//!
//! All variants ultimately resolve to:
//!
//! 1. A `(lang, page_id)` pair.
//! 2. One or two rev_ids to render (or "latest" — derived from the
//!    article's `last_processed_revid`).
//! 3. A `ResponseParameters` derived from query string.
//!
//! Then we call [`wikiwho_attribute::response::build_rev_content`] on
//! a [`SnapshotReader`]-hydrated `Article` and return the JSON.
//!
//! Endpoint 1 (`/rev_content/rev_id/{rev_id}/`) resolves the rev_id to
//! a page_id via the per-language `rev_id_index.bin` sidecar (loaded
//! lazily by [`AppState::resolve_rev_id`]), then re-uses the same
//! page_id path as endpoints 4/6.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use wikiwho_attribute::response::{
    RevContentError, RevContentResponse, ResponseParameters, build_rev_content,
};
use wikiwho_storage::reader::SnapshotReader;

use crate::error::ServerError;
use crate::params::RawTokenParams;
use crate::state::AppState;

/// Path params accepted by endpoint 1: `/{lang}/api/{ver}/rev_content/rev_id/{rev_id}/`.
#[derive(serde::Deserialize)]
pub struct RevIdPath {
    pub lang: String,
    #[allow(dead_code)] // version segment kept for future per-version branching
    pub version: String,
    pub rev_id: u64,
}

/// Path params accepted by endpoint 4: `/{lang}/api/{ver}/rev_content/page_id/{page_id}/`
/// and endpoint 6: `/{lang}/api/{ver}/latest_rev_content/page_id/{page_id}/`.
#[derive(serde::Deserialize)]
pub struct PageIdPath {
    pub lang: String,
    #[allow(dead_code)]
    pub version: String,
    pub page_id: u64,
}

/// Path params for endpoint 2 / 5 (title-only) — latest revision.
#[derive(serde::Deserialize)]
pub struct TitlePath {
    pub lang: String,
    #[allow(dead_code)]
    pub version: String,
    pub title: String,
}

/// Path params for endpoint 3 (title + specific rev_id).
#[derive(serde::Deserialize)]
pub struct TitleRevPath {
    pub lang: String,
    #[allow(dead_code)]
    pub version: String,
    pub title: String,
    pub rev_id: u64,
}

/// Endpoint 1: rev_content by rev_id only.
///
/// Resolves the rev_id to a page_id via the per-language
/// `rev_id_index.bin` sidecar, then delegates to the standard page_id
/// path with `target_rev_id = Some(rev_id)`. If the index has no entry
/// for this rev_id we return the "still processing" envelope so the
/// Impact Visualizer client treats it as "skip this article".
pub async fn rev_content_by_rev_id(
    State(state): State<AppState>,
    Path(path): Path<RevIdPath>,
    Query(params): Query<RawTokenParams>,
) -> Response {
    let Some(page_id) = state.resolve_rev_id(&path.lang, path.rev_id) else {
        tracing::debug!(
            lang = %path.lang,
            rev_id = path.rev_id,
            "rev_id not present in rev_id_index.bin"
        );
        return still_processing();
    };
    handle_page_id(state, &path.lang, page_id, Some(path.rev_id), params).await
}

/// Endpoint 4 + 6: latest revision by page_id (also handles the case
/// where the route was `rev_content/page_id/.../` — the wire format is
/// the same).
pub async fn rev_content_by_page_id(
    State(state): State<AppState>,
    Path(path): Path<PageIdPath>,
    Query(params): Query<RawTokenParams>,
) -> Response {
    handle_page_id(state, &path.lang, path.page_id, None, params).await
}

/// Endpoint 2 + 5: latest revision by title.
pub async fn rev_content_by_title(
    State(state): State<AppState>,
    Path(path): Path<TitlePath>,
    Query(params): Query<RawTokenParams>,
) -> Response {
    let title = normalize_title(&path.title);
    let Some(page_id) = state.resolve_title(&path.lang, &title) else {
        return title_not_found(&path.lang, &title);
    };
    handle_page_id(state, &path.lang, page_id, None, params).await
}

/// Endpoint 3: specific rev_id of a specifically-titled article.
///
/// The 5-digit-rev-id heuristic that disambiguates titles-with-slash
/// from titles-without-slash lives in the route definition (see
/// `routes.rs`); by the time we get here, the segments are already
/// split.
pub async fn rev_content_by_title_rev(
    State(state): State<AppState>,
    Path(path): Path<TitleRevPath>,
    Query(params): Query<RawTokenParams>,
) -> Response {
    let title = normalize_title(&path.title);
    let Some(page_id) = state.resolve_title(&path.lang, &title) else {
        return title_not_found(&path.lang, &title);
    };
    handle_page_id(state, &path.lang, page_id, Some(path.rev_id), params).await
}

/// Core handler shared by all variants. `target_rev_id` of `None`
/// selects the latest processed revision.
async fn handle_page_id(
    state: AppState,
    lang: &str,
    page_id: u64,
    target_rev_id: Option<u64>,
    params: RawTokenParams,
) -> Response {
    let response_params = params.into_response_parameters();

    let reader = match SnapshotReader::open(state.storage_root(), lang, page_id) {
        Ok(reader) => reader,
        Err(wikiwho_storage::StorageError::Io(io_err))
            if io_err.kind() == std::io::ErrorKind::NotFound =>
        {
            return still_processing();
        }
        Err(err) => {
            return error_500(ServerError::from(err));
        }
    };

    let target = match target_rev_id {
        Some(id) => id,
        None => {
            let Some(last) = reader.article.ordered_revisions.last().copied() else {
                return still_processing();
            };
            last
        }
    };

    render(&reader.article, target, response_params)
}

/// Render `build_rev_content` output as an HTTP response. Encapsulates
/// the OK/error envelope split.
fn render(
    article: &wikiwho_attribute::structures::Article,
    rev_id: u64,
    params: ResponseParameters,
) -> Response {
    match build_rev_content(article, &[rev_id], params) {
        Ok(body) => render_success(&body),
        Err(err) => render_error(&err),
    }
}

fn render_success(body: &RevContentResponse) -> Response {
    match serde_json::to_value(body) {
        Ok(value) => (StatusCode::OK, Json(value)).into_response(),
        Err(err) => error_500(ServerError::from(err)),
    }
}

fn render_error(err: &RevContentError) -> Response {
    match serde_json::to_value(err) {
        Ok(value) => (StatusCode::BAD_REQUEST, Json(value)).into_response(),
        Err(err) => error_500(ServerError::from(err)),
    }
}

/// "Still processing" envelope — API.md §1 "Response (200,
/// not-yet-processed)". The Impact Visualizer client treats 408 as
/// "skip this article".
fn still_processing() -> Response {
    let body = json!({
        "Info": "Process took more than 240 seconds. Requested data will be available soon (Max 300 seconds). Please try again later."
    });
    (StatusCode::REQUEST_TIMEOUT, Json(body)).into_response()
}

fn title_not_found(lang: &str, title: &str) -> Response {
    // Title that isn't on disk maps to the same envelope as
    // "still processing" — we can't distinguish "not started" from
    // "in progress" without a stronger ingest signal.
    tracing::debug!(lang = %lang, title = %title, "title not found in index");
    still_processing()
}

fn error_500(err: ServerError) -> Response {
    tracing::error!(error = %err, "server error");
    let body: Value = json!({
        "error": err.to_string(),
        "success": false,
        "rev_id": null,
        "page_title": null,
    });
    (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
}

/// MediaWiki wiki titles use underscores; URL paths can use either.
/// Normalize spaces (raw or %20-decoded by axum) to underscores so
/// the lookup matches what `wikiwho-mwclient` stores.
fn normalize_title(raw: &str) -> String {
    raw.replace(' ', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_title_replaces_spaces() {
        assert_eq!(normalize_title("Barack Obama"), "Barack_Obama");
        assert_eq!(normalize_title("Already_Under_Scored"), "Already_Under_Scored");
        assert_eq!(normalize_title(""), "");
    }
}
