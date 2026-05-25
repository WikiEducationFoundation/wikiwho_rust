//! HTTP service for the rewritten wikiwho-api.
//!
//! Serves the wire-format documented in `API.md` atop
//! `wikiwho-storage`. Read-only at first cut — the cache-miss path
//! (fetching revisions from MW and replaying through the algorithm)
//! is a follow-up. If a request hits an article not on disk we
//! respond with the "still processing" envelope from API.md §1 so the
//! consumers' existing retry logic kicks in.
//!
//! Routes follow API.md §1-8 (rev_content + WhoColor). Ephemeral
//! non-mainspace (§9) is not implemented yet. The router also accepts
//! both `v1.0.0` and `v1.0.0-beta` as version-segment aliases per
//! API.md §"Versioning".

pub mod cache_miss;
pub mod error;
pub mod handlers;
pub mod index;
pub mod params;
pub mod routes;
pub mod state;
pub mod whocolor_html;
pub mod whocolor_template_strip;
pub mod whocolor_wikitext;

pub use error::ServerError;
pub use routes::router;
pub use state::AppState;
