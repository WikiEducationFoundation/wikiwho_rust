//! Integration tests for `resolve_users` and `fetch_parsoid_html`
//! against tiny axum mocks. Keeps the test corpus offline-runnable.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::Query;
use axum::http::StatusCode;
use axum::{Json, Router, response::IntoResponse, routing::get};
use serde_json::{Value, json};
use wikiwho_mwclient::MwClientBuilder;

#[derive(Clone)]
struct UsersMock {
    db: Arc<HashMap<u64, String>>,
    seen_requests: Arc<std::sync::Mutex<Vec<Vec<u64>>>>,
}

async fn users_handler(
    axum::extract::State(state): axum::extract::State<UsersMock>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    // Action API: parse `ususerids=ID|ID|ID`.
    let ids: Vec<u64> = params
        .get("ususerids")
        .map(|s| {
            s.split('|')
                .filter_map(|x| x.parse::<u64>().ok())
                .collect()
        })
        .unwrap_or_default();
    state.seen_requests.lock().unwrap().push(ids.clone());

    let users: Vec<Value> = ids
        .iter()
        .map(|id| match state.db.get(id) {
            Some(name) => json!({"userid": id, "name": name}),
            None => json!({"userid": id, "missing": true}),
        })
        .collect();
    Json(json!({
        "batchcomplete": true,
        "query": { "users": users }
    }))
}

async fn spawn_users_mock(db: HashMap<u64, String>) -> (String, Arc<std::sync::Mutex<Vec<Vec<u64>>>>) {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let state = UsersMock {
        db: Arc::new(db),
        seen_requests: seen.clone(),
    };
    let app = Router::new()
        .route("/w/api.php", get(users_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/w/api.php"), seen)
}

#[tokio::test]
async fn resolve_users_returns_id_to_name_map() {
    let mut db = HashMap::new();
    db.insert(1465, "Froderik".to_string());
    db.insert(5959, "Valery Beaud".to_string());

    let (api_url, _seen) = spawn_users_mock(db).await;
    let client = MwClientBuilder::for_api_url(api_url)
        .between_batches(Duration::from_millis(1))
        .build()
        .unwrap();
    let names = client.resolve_users(&[1465, 5959]).await.unwrap();
    assert_eq!(names.len(), 2);
    assert_eq!(names.get(&1465).map(String::as_str), Some("Froderik"));
    assert_eq!(names.get(&5959).map(String::as_str), Some("Valery Beaud"));
}

#[tokio::test]
async fn resolve_users_skips_missing_ids_in_response() {
    let mut db = HashMap::new();
    db.insert(1, "A".to_string());
    let (api_url, _) = spawn_users_mock(db).await;
    let client = MwClientBuilder::for_api_url(api_url)
        .between_batches(Duration::from_millis(1))
        .build()
        .unwrap();
    let names = client.resolve_users(&[1, 2, 3]).await.unwrap();
    assert_eq!(names.len(), 1);
    assert_eq!(names.get(&1).map(String::as_str), Some("A"));
}

#[tokio::test]
async fn resolve_users_batches_over_50() {
    // 51 ids → 2 batches (50 + 1).
    let mut db = HashMap::new();
    for i in 1..=51u64 {
        db.insert(i, format!("u{i}"));
    }
    let (api_url, seen) = spawn_users_mock(db).await;
    let client = MwClientBuilder::for_api_url(api_url)
        .between_batches(Duration::from_millis(1))
        .build()
        .unwrap();
    let ids: Vec<u64> = (1..=51).collect();
    let names = client.resolve_users(&ids).await.unwrap();
    assert_eq!(names.len(), 51);
    let batches = seen.lock().unwrap();
    assert_eq!(batches.len(), 2, "expected exactly 2 batches");
    assert_eq!(batches[0].len(), 50);
    assert_eq!(batches[1].len(), 1);
}

#[tokio::test]
async fn resolve_users_dedupes_input() {
    let mut db = HashMap::new();
    db.insert(7, "Lucky".to_string());
    let (api_url, seen) = spawn_users_mock(db).await;
    let client = MwClientBuilder::for_api_url(api_url)
        .between_batches(Duration::from_millis(1))
        .build()
        .unwrap();
    let _ = client.resolve_users(&[7, 7, 7]).await.unwrap();
    let batches = seen.lock().unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0], vec![7]);
}

#[tokio::test]
async fn resolve_users_empty_input_returns_empty() {
    // Don't even hit the network — confirm zero-cost short-circuit.
    let client = MwClientBuilder::for_api_url("http://127.0.0.1:1/w/api.php")
        .request_timeout(Duration::from_millis(10))
        .retry_max_attempts(1)
        .build()
        .unwrap();
    let names = client.resolve_users(&[]).await.unwrap();
    assert!(names.is_empty());
}

// ---- Parsoid HTML mock ----

async fn parsoid_ok_handler() -> impl IntoResponse {
    "<html><body><p>hello world</p></body></html>"
}

async fn parsoid_404_handler() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "type": "MediaWikiError/Not_Found",
            "title": "rest-nonexistent-revision",
            "detail": "The specified revision (9999) does not exist"
        })),
    )
}

#[tokio::test]
async fn fetch_parsoid_html_returns_body_on_200() {
    let app = Router::new().route("/page/html/{title}/{rev_id}", get(parsoid_ok_handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = MwClientBuilder::for_api_url("http://example.invalid/w/api.php")
        .rest_base_url(format!("http://{addr}"))
        .build()
        .unwrap();
    let html = client.fetch_parsoid_html("Cat", 12345).await.unwrap();
    assert!(html.contains("hello world"));
}

#[tokio::test]
async fn fetch_parsoid_html_translates_404_to_page_missing() {
    let app = Router::new().route("/page/html/{title}/{rev_id}", get(parsoid_404_handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = MwClientBuilder::for_api_url("http://example.invalid/w/api.php")
        .rest_base_url(format!("http://{addr}"))
        .build()
        .unwrap();
    let err = client.fetch_parsoid_html("Cat", 9999).await.unwrap_err();
    assert!(matches!(err, wikiwho_mwclient::MwError::PageMissing { .. }));
}

// ---- action=parse HTML mock ----

async fn action_parse_handler(
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> impl IntoResponse {
    if params.get("action").map(|s| s.as_str()) != Some("parse") {
        return Json(json!({"error": {"code": "wrongaction"}}));
    }
    // formatversion=2 → parse.text is a flat string.
    Json(json!({
        "parse": {
            "title": "Cat",
            "pageid": 1,
            "revid": params.get("oldid").and_then(|s| s.parse::<u64>().ok()).unwrap_or(0),
            "text": "<div class=\"mw-content-ltr mw-parser-output\"><p>hello world</p></div>"
        }
    }))
}

async fn action_parse_missing_rev_handler() -> impl IntoResponse {
    Json(json!({
        "error": {
            "code": "nosuchrevid",
            "info": "There is no revision with ID 9999."
        }
    }))
}

#[tokio::test]
async fn fetch_rendered_html_extracts_parse_text() {
    let app = Router::new().route("/w/api.php", get(action_parse_handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = MwClientBuilder::for_api_url(format!("http://{addr}/w/api.php"))
        .build()
        .unwrap();
    let html = client.fetch_rendered_html(12345).await.unwrap();
    assert!(html.starts_with("<div class=\"mw-content-ltr mw-parser-output\""));
    assert!(html.contains("hello world"));
}

#[tokio::test]
async fn fetch_rendered_html_translates_nosuchrevid_to_page_missing() {
    let app = Router::new().route("/w/api.php", get(action_parse_missing_rev_handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = MwClientBuilder::for_api_url(format!("http://{addr}/w/api.php"))
        .build()
        .unwrap();
    let err = client.fetch_rendered_html(9999).await.unwrap_err();
    assert!(matches!(err, wikiwho_mwclient::MwError::PageMissing { .. }));
}

#[tokio::test]
async fn fetch_parsoid_html_percent_encodes_title() {
    // Pick a title with characters that require percent-encoding so we
    // can sanity-check that the URL composition doesn't break.
    // We don't have to actually call MW — instead inspect the URL the
    // client would form via the public api: by routing it at a mock
    // that just echoes back the requested path.
    async fn echo_path(
        axum::extract::Path((title, rev_id)): axum::extract::Path<(String, String)>,
    ) -> impl IntoResponse {
        format!("title={title};rev_id={rev_id}")
    }
    let app = Router::new().route("/page/html/{title}/{rev_id}", get(echo_path));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = MwClientBuilder::for_api_url("http://example.invalid/w/api.php")
        .rest_base_url(format!("http://{addr}"))
        .build()
        .unwrap();
    // Title with a slash + non-ASCII.
    let echo = client.fetch_parsoid_html("中国/Test", 7).await.unwrap();
    // axum decodes the path back to UTF-8 before passing to the handler;
    // the title should round-trip.
    assert!(echo.contains("title=中国/Test"));
    assert!(echo.contains("rev_id=7"));
}
