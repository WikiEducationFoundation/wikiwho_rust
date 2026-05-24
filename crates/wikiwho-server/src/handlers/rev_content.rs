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
//! **Cache-miss path (PLAN.md §280-287):** for endpoints 1-6, if the
//! article isn't on disk the handler does a one-shot MW lookup to
//! learn `(title, page_id, last_revid)`, spawns a background task to
//! fetch + replay the full history, and returns the
//! "still processing" envelope (HTTP 408 — API.md §1) immediately.
//! Subsequent requests for the same article either see the
//! in-flight slot and 408 again, or — once processing finishes —
//! serve from disk. Endpoint 1 (rev_id-only) does an extra
//! `revids=` lookup to learn the article from the rev_id; the
//! cache-miss task then fetches revisions up through the requested
//! rev_id (not the article's live tip), matching endpoint 3's
//! semantics.

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
use wikiwho_mwclient::{MwClient, MwError};
use wikiwho_storage::reader::SnapshotReader;

use crate::cache_miss;
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

/// Cache-miss seed handed to [`trigger_cache_miss`]. Captures
/// everything the background task needs to do its work — the
/// `page_id` it lives under is implicit at the call site.
#[derive(Debug, Clone)]
struct CacheMissPlan {
    title: String,
    end_rev_id: u64,
}

/// Endpoint 1: rev_content by rev_id only.
///
/// Three paths:
///
/// 1. rev_id is in the per-language `rev_id_index.bin` sidecar →
///    delegate to the on-disk render with the requested rev_id as the
///    target (matches endpoint 3's "title + rev_id" snapshot
///    semantics).
/// 2. rev_id is not in the sidecar → ask MW to map
///    `rev_id → (title, page_id)` via `prop=info&revids=...`, then
///    spawn the cache-miss task with `end_rev_id = rev_id` so the
///    fetched history terminates at the requested snapshot.
/// 3. MW says the rev_id doesn't exist (`badrevids`) or any other
///    failure → return the 408 still-processing envelope.
pub async fn rev_content_by_rev_id(
    State(state): State<AppState>,
    Path(path): Path<RevIdPath>,
    Query(params): Query<RawTokenParams>,
) -> Response {
    let response_params = params.into_response_parameters();
    let target_rev_id = Some(path.rev_id);

    // 1. Fast path: rev_id is in the sidecar → page_id known, try
    //    on-disk.
    if let Some(page_id) = state.resolve_rev_id(&path.lang, path.rev_id) {
        if let Some(resp) =
            try_serve_from_disk(&state, &path.lang, page_id, target_rev_id, response_params)
        {
            return resp;
        }
        // Indexed but file missing — log + fall through to MW so we
        // can rebuild.
        tracing::warn!(
            lang = %path.lang,
            page_id = page_id,
            rev_id = path.rev_id,
            "rev_id indexed but article files missing; will refetch via MW"
        );
    }

    // 2. Cache miss: ask MW for the (title, page_id) the rev_id
    //    belongs to. Override `last_revid` with the requested rev_id so
    //    the fetched history terminates at the requested snapshot.
    let plan_with_page_id = resolve_via_mw(&state, &path.lang, |mw| {
        let rev_id = path.rev_id;
        async move {
            let info = mw.resolve_rev_id(rev_id).await?;
            Ok(wikiwho_mwclient::PageInfo {
                title: info.title,
                page_id: info.page_id,
                last_revid: rev_id,
            })
        }
    })
    .await;

    let Some((page_id, plan)) = plan_with_page_id else {
        return still_processing();
    };

    // MW gave us the canonical (title, page_id) — try on-disk one more
    // time in case the article was processed but the rev_id index hadn't
    // been refreshed.
    if let Some(resp) =
        try_serve_from_disk(&state, &path.lang, page_id, target_rev_id, response_params)
    {
        if let Err(e) = state.refresh_rev_id_index(&path.lang) {
            tracing::warn!(lang = %path.lang, error = %e, "rev_id-index refresh failed");
        }
        return resp;
    }
    cache_miss_response(state, path.lang, page_id, Some(plan))
}

/// Endpoint 4 + 6: latest revision by page_id.
///
/// On cache miss, asks MW for `(title, last_revid)` and spawns the
/// background processor.
pub async fn rev_content_by_page_id(
    State(state): State<AppState>,
    Path(path): Path<PageIdPath>,
    Query(params): Query<RawTokenParams>,
) -> Response {
    let response_params = params.into_response_parameters();
    let page_id = path.page_id;

    // Try on-disk first — cheap when the article is already processed.
    if let Some(resp) = try_serve_from_disk(&state, &path.lang, page_id, None, response_params) {
        return resp;
    }

    // Cache miss: ask MW for the page's metadata so we know what
    // rev_id to fetch through.
    let plan = resolve_via_mw(&state, &path.lang, |mw| async move {
        mw.resolve_page_id(page_id).await
    })
    .await
    .map(|(_pid_from_mw, plan)| plan);
    cache_miss_response(state, path.lang, page_id, plan)
}

/// Endpoint 2 + 5: latest revision by title.
pub async fn rev_content_by_title(
    State(state): State<AppState>,
    Path(path): Path<TitlePath>,
    Query(params): Query<RawTokenParams>,
) -> Response {
    let title = normalize_title(&path.title);
    let response_params = params.into_response_parameters();

    if let Some(page_id) = state.resolve_title(&path.lang, &title) {
        if let Some(resp) = try_serve_from_disk(&state, &path.lang, page_id, None, response_params)
        {
            return resp;
        }
        // Title in index, file vanished — log + fall through to MW so
        // we can refetch.
        tracing::warn!(
            lang = %path.lang,
            title = %title,
            page_id = page_id,
            "title indexed but article files missing; will refetch"
        );
    }

    // Title not in index OR on-disk read failed: ask MW.
    let plan_with_page_id = resolve_via_mw(&state, &path.lang, |mw| {
        let title = title.clone();
        async move { mw.resolve_title(&title).await }
    })
    .await;

    let Some((page_id, plan)) = plan_with_page_id else {
        return still_processing();
    };

    // MW gave us the canonical page_id — try on-disk one more time in
    // case the article was processed under a different title casing.
    if let Some(resp) = try_serve_from_disk(&state, &path.lang, page_id, None, response_params) {
        // Backfill the title index so subsequent requests skip MW.
        if let Err(e) = state.refresh_title_index(&path.lang) {
            tracing::warn!(lang = %path.lang, error = %e, "title-index refresh failed");
        }
        return resp;
    }
    cache_miss_response(state, path.lang, page_id, Some(plan))
}

/// Endpoint 3: specific rev_id of a specifically-titled article.
///
/// On cache miss, the spawned task fetches revisions **through the
/// requested rev_id**, not the article's current latest.
pub async fn rev_content_by_title_rev(
    State(state): State<AppState>,
    Path(path): Path<TitleRevPath>,
    Query(params): Query<RawTokenParams>,
) -> Response {
    let title = normalize_title(&path.title);
    let response_params = params.into_response_parameters();
    let target_rev_id = Some(path.rev_id);

    if let Some(page_id) = state.resolve_title(&path.lang, &title) {
        if let Some(resp) =
            try_serve_from_disk(&state, &path.lang, page_id, target_rev_id, response_params)
        {
            return resp;
        }
        tracing::warn!(
            lang = %path.lang,
            title = %title,
            page_id = page_id,
            "title indexed but article files missing; will refetch"
        );
    }

    // Cache miss: resolve title via MW. Use path.rev_id as end_rev_id
    // (not the article's latest) so the fetched history matches the
    // requested snapshot.
    let plan_with_page_id = resolve_via_mw(&state, &path.lang, |mw| {
        let title = title.clone();
        async move {
            let info = mw.resolve_title(&title).await?;
            Ok(wikiwho_mwclient::PageInfo {
                title: info.title,
                page_id: info.page_id,
                last_revid: path.rev_id,
            })
        }
    })
    .await;

    let Some((page_id, plan)) = plan_with_page_id else {
        return still_processing();
    };
    if let Some(resp) =
        try_serve_from_disk(&state, &path.lang, page_id, target_rev_id, response_params)
    {
        if let Err(e) = state.refresh_title_index(&path.lang) {
            tracing::warn!(lang = %path.lang, error = %e, "title-index refresh failed");
        }
        return resp;
    }
    cache_miss_response(state, path.lang, page_id, Some(plan))
}

/// Render the article from disk, with the requested rev_id (or
/// latest if `target_rev_id` is `None`). Returns `None` if the
/// article isn't on disk so the caller can fall through to a
/// cache-miss path; returns `Some(error_500)` if the storage layer
/// surfaced any non-NotFound error.
fn try_serve_from_disk(
    state: &AppState,
    lang: &str,
    page_id: u64,
    target_rev_id: Option<u64>,
    params: ResponseParameters,
) -> Option<Response> {
    let reader = match SnapshotReader::open(state.storage_root(), lang, page_id) {
        Ok(r) => r,
        Err(wikiwho_storage::StorageError::Io(io)) if io.kind() == std::io::ErrorKind::NotFound => {
            return None;
        }
        Err(err) => return Some(error_500(ServerError::from(err))),
    };
    let target = match target_rev_id {
        Some(id) => id,
        None => reader.article.ordered_revisions.last().copied()?,
    };
    Some(render(&reader.article, target, params))
}

/// Construct a [`CacheMissPlan`] by asking MW for the article's
/// `(title, page_id, last_revid)`. Returns `None` for any MW failure
/// — those map to the 408 envelope at the call site.
async fn resolve_via_mw<F, Fut>(
    state: &AppState,
    language: &str,
    op: F,
) -> Option<(u64, CacheMissPlan)>
where
    F: FnOnce(std::sync::Arc<MwClient>) -> Fut,
    Fut: std::future::Future<Output = Result<wikiwho_mwclient::PageInfo, MwError>>,
{
    let mw = match state.mw_client(language) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(lang = %language, error = %e, "mw client unavailable");
            return None;
        }
    };
    match op(mw).await {
        Ok(info) => Some((
            info.page_id,
            CacheMissPlan {
                // MW echoes titles back with spaces; our TitleIndex
                // keys on the underscored form that URL lookups
                // produce. Normalize before storing so the next
                // request hits the on-disk article instead of
                // re-spawning the same cache-miss.
                title: normalize_title(&info.title),
                end_rev_id: info.last_revid,
            },
        )),
        Err(MwError::PageMissing { page_id }) => {
            tracing::debug!(lang = %language, page_id = page_id, "page missing on MW");
            None
        }
        Err(e) => {
            tracing::warn!(lang = %language, error = %e, "MW lookup failed");
            None
        }
    }
}

/// Spawn the cache-miss task if we won the in-flight race; either way
/// return the still-processing envelope. Spawning requires an
/// `MwClient` (we already have it cached at this point, since
/// `resolve_via_mw` succeeded).
fn cache_miss_response(
    state: AppState,
    language: String,
    page_id: u64,
    plan: Option<CacheMissPlan>,
) -> Response {
    if let Some(plan) = plan {
        trigger_cache_miss(state, language, page_id, plan);
    }
    still_processing()
}

fn trigger_cache_miss(
    state: AppState,
    language: String,
    page_id: u64,
    plan: CacheMissPlan,
) {
    let mw = match state.mw_client(&language) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(lang = %language, error = %e, "mw client unavailable; not spawning");
            return;
        }
    };
    if !state.try_claim_in_flight(&language, page_id) {
        // Another request beat us to it; the existing task will
        // process and persist on its own.
        tracing::debug!(
            lang = %language,
            page_id = page_id,
            "cache-miss already in flight"
        );
        return;
    }
    let CacheMissPlan { title, end_rev_id } = plan;
    let fetcher = async move {
        let fetcher = mw.fetch_revisions(page_id, end_rev_id);
        cache_miss::collect_all_revisions(fetcher)
            .await
            .map_err(Into::into)
    };
    tracing::info!(
        lang = %language,
        page_id = page_id,
        end_rev_id = end_rev_id,
        "spawning cache-miss task"
    );
    let _handle = state.spawn_cache_miss(language, title, page_id, fetcher);
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
