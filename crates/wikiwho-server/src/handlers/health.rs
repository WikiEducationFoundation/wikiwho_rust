//! Liveness probe endpoint.
//!
//! Not part of `API.md` — this exists so nginx (and humans) can cheaply
//! confirm the binary is up without going through any storage or MW
//! lookup path. Returns the package version and the storage-root path
//! so an operator can sanity-check that the running binary is the one
//! they think it is.

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::state::AppState;

/// Build-time package version (Cargo's `CARGO_PKG_VERSION` env var, set
/// at compile time from `Cargo.toml`).
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
    pub storage_root: String,
}

/// `GET /healthz` — returns 200 with the version + storage root.
pub async fn healthz(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: VERSION,
        storage_root: state.storage_root().display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn app() -> Router {
        let tmp = tempfile::tempdir().unwrap().keep();
        let state = AppState::new(tmp);
        Router::new()
            .route("/healthz", axum::routing::get(healthz))
            .with_state(state)
    }

    #[tokio::test]
    async fn healthz_returns_200_with_version() {
        let resp = app()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["status"], "ok");
        assert_eq!(parsed["version"], VERSION);
        assert!(
            parsed["storage_root"].as_str().is_some(),
            "storage_root should be a string"
        );
    }
}
