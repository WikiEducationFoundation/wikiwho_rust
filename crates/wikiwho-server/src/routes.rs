//! Axum router. Mirrors the URL patterns in `API.md §"Endpoints"` and
//! `API.md §"URL routing quirks"`.
//!
//! The `{version}` path segment matches `v1.0.0` OR `v1.0.0-beta`
//! (API.md §"Versioning") — we route both through the same handlers.
//!
//! Endpoint 3 (`/rev_content/{title}/{rev_id}/`) carries the "5-digit
//! rev_id" quirk from `api/urls.py:25`: titles can contain `/`, so the
//! router must require the rev_id to have at least 5 digits. Without
//! it `Foo/Bar` and `Foo/12345` would be ambiguous. We can't enforce a
//! regex constraint on axum's path syntax directly; the handler
//! falls back to the title-only path when the second segment doesn't
//! parse as `u64` with ≥5 digits — see the parsing in
//! `handlers::rev_content::rev_content_by_title_rev`.

use axum::Router;
use axum::routing::get;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::handlers::{health, rev_content, whocolor};
use crate::state::AppState;

/// Build the application router rooted at the wiki language segment.
pub fn router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let trace = TraceLayer::new_for_http()
        .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
        .on_response(DefaultOnResponse::new().level(Level::INFO));

    Router::new()
        // Liveness probe (not part of API.md).
        .route("/healthz", get(health::healthz))
        // Endpoint 1 — `/{lang}/api/{version}/rev_content/rev_id/{rev_id}/`
        .route(
            "/{lang}/api/{version}/rev_content/rev_id/{rev_id}/",
            get(rev_content::rev_content_by_rev_id),
        )
        // Endpoint 4 — `/{lang}/api/{version}/rev_content/page_id/{page_id}/`
        .route(
            "/{lang}/api/{version}/rev_content/page_id/{page_id}/",
            get(rev_content::rev_content_by_page_id),
        )
        // Endpoint 6 — `/{lang}/api/{version}/latest_rev_content/page_id/{page_id}/`
        // (alias of endpoint 4)
        .route(
            "/{lang}/api/{version}/latest_rev_content/page_id/{page_id}/",
            get(rev_content::rev_content_by_page_id),
        )
        // Endpoint 3 — `/{lang}/api/{version}/rev_content/{title}/{rev_id}/`
        .route(
            "/{lang}/api/{version}/rev_content/{title}/{rev_id}/",
            get(rev_content::rev_content_by_title_rev),
        )
        // Endpoint 2 — `/{lang}/api/{version}/rev_content/{title}/`
        .route(
            "/{lang}/api/{version}/rev_content/{title}/",
            get(rev_content::rev_content_by_title),
        )
        // Endpoint 5 — `/{lang}/api/{version}/latest_rev_content/{title}/`
        // (alias of endpoint 2)
        .route(
            "/{lang}/api/{version}/latest_rev_content/{title}/",
            get(rev_content::rev_content_by_title),
        )
        // Endpoint 7 — `/{lang}/whocolor/{version}/{title}/{rev_id}/`
        // (per API.md §7; `rev_id == 0` is the slash-in-title
        // workaround → handler routes to latest).
        .route(
            "/{lang}/whocolor/{version}/{title}/{rev_id}/",
            get(whocolor::whocolor_by_title_rev),
        )
        // Endpoint 8 — `/{lang}/whocolor/{version}/{title}/`
        // (latest revision).
        .route(
            "/{lang}/whocolor/{version}/{title}/",
            get(whocolor::whocolor_by_title_latest),
        )
        .with_state(state)
        .layer(cors)
        .layer(trace)
}
