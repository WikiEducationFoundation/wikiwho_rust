//! End-to-end test of the WhoColor endpoint against a mock MW that
//! serves both Action API (`prop=info`, `prop=revisions`,
//! `list=users`) and Parsoid REST (`/page/html/{title}/{rev_id}`).
//!
//! Fixture: zh/1686258 中国 (7 revs). Asserts the response envelope
//! has the documented top-level keys and at least one injected span.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Path as AxumPath, Query, State},
    response::IntoResponse,
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use wikiwho_mwclient::MwClientBuilder;
use wikiwho_server::{AppState, router};

#[derive(Debug, Deserialize, Clone)]
struct HistoryEntry {
    rev_id: u64,
    timestamp: String,
    sha1: Option<String>,
    comment: Option<String>,
    minor: bool,
    user_id: Option<u64>,
    user_name: Option<String>,
    text: String,
    #[allow(dead_code)]
    text_hidden: bool,
}

#[derive(Debug, Deserialize, Clone)]
struct FixtureMeta {
    lang: String,
    title: String,
    page_id: u64,
    rev_id: u64,
}

#[derive(Clone)]
struct Fixture {
    meta: FixtureMeta,
    history: Vec<HistoryEntry>,
}

fn fixture_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.join("parity-fixtures")
}

fn load_fixture(rel: &str) -> Option<Fixture> {
    let dir = fixture_root().join(rel);
    let meta_path = dir.join("meta.json");
    let history_path = dir.join("history.jsonl");
    if !meta_path.exists() || !history_path.exists() {
        eprintln!("skipping {rel}: fixture not present");
        return None;
    }
    let meta: FixtureMeta =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    let history: Vec<HistoryEntry> = std::fs::read_to_string(&history_path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    Some(Fixture { meta, history })
}

async fn spawn_server(state: AppState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

async fn wait_for_processing(state: &AppState, lang: &str, page_id: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while state.is_in_flight(lang, page_id) {
        if std::time::Instant::now() > deadline {
            panic!("cache-miss task did not complete within 30s");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---- Mock MW (Action API + Parsoid REST) -------------------------

#[derive(Clone)]
struct MockMw {
    info: Arc<Value>,
    revisions: Arc<Value>,
    parsoid_html: Arc<String>,
    user_names: Arc<HashMap<u64, String>>,
}

fn build_info(fixture: &Fixture) -> Value {
    json!({
        "batchcomplete": true,
        "query": {
            "pages": [{
                "pageid": fixture.meta.page_id,
                "ns": 0,
                "title": fixture.meta.title,
                "lastrevid": fixture.meta.rev_id,
            }]
        }
    })
}

fn build_revisions(fixture: &Fixture) -> Value {
    let revisions: Vec<Value> = fixture
        .history
        .iter()
        .map(|h| {
            let mut r = json!({
                "revid": h.rev_id,
                "parentid": 0u64,
                "timestamp": h.timestamp,
                "minor": h.minor,
            });
            if let Some(u) = h.user_name.as_ref() {
                r["user"] = json!(u);
            }
            if let Some(id) = h.user_id {
                r["userid"] = json!(id);
            }
            if let Some(c) = h.comment.as_ref() {
                r["comment"] = json!(c);
            }
            if let Some(s) = h.sha1.as_ref() {
                r["sha1"] = json!(s);
            }
            r["slots"] = json!({
                "main": { "contentmodel": "wikitext", "content": h.text }
            });
            r
        })
        .collect();
    json!({
        "batchcomplete": true,
        "query": {
            "pages": [{
                "pageid": fixture.meta.page_id,
                "ns": 0,
                "title": fixture.meta.title,
                "revisions": revisions,
            }]
        }
    })
}

async fn action_handler(
    State(state): State<MockMw>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    match params.get("prop").map(String::as_str) {
        Some("info") => Json((*state.info).clone()),
        Some("revisions") => Json((*state.revisions).clone()),
        _ => {
            // list=users?
            if params.get("list").map(String::as_str) == Some("users") {
                let ids = params
                    .get("ususerids")
                    .map(|s| {
                        s.split('|')
                            .filter_map(|x| x.parse::<u64>().ok())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let users: Vec<Value> = ids
                    .iter()
                    .map(|id| match state.user_names.get(id) {
                        Some(n) => json!({"userid": id, "name": n}),
                        None => json!({"userid": id, "missing": true}),
                    })
                    .collect();
                return Json(json!({
                    "batchcomplete": true,
                    "query": { "users": users }
                }));
            }
            Json(json!({"error": {"code": "unhandled", "info": "mock doesn't speak this"}}))
        }
    }
}

async fn parsoid_handler(
    State(state): State<MockMw>,
    AxumPath((_title, _rev_id)): AxumPath<(String, String)>,
) -> impl IntoResponse {
    (*state.parsoid_html).clone()
}

async fn spawn_mock_mw(
    fixture: Fixture,
    parsoid_html: String,
    user_names: HashMap<u64, String>,
) -> (String, String) {
    let state = MockMw {
        info: Arc::new(build_info(&fixture)),
        revisions: Arc::new(build_revisions(&fixture)),
        parsoid_html: Arc::new(parsoid_html),
        user_names: Arc::new(user_names),
    };
    let app = Router::new()
        .route("/w/api.php", get(action_handler))
        .route("/api/rest_v1/page/html/{title}/{rev_id}", get(parsoid_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let action = format!("http://{addr}/w/api.php");
    let rest = format!("http://{addr}/api/rest_v1");
    (action, rest)
}

// ------------------------------------------------------------------

#[tokio::test]
async fn whocolor_cache_miss_persists_then_serves() {
    let Some(fixture) = load_fixture("zh/1686258/64806634") else {
        return;
    };
    let lang = fixture.meta.lang.clone();
    let title = fixture.meta.title.clone();
    let page_id = fixture.meta.page_id;
    let rev_id = fixture.meta.rev_id;

    let parsoid_html =
        "<html><body><p>中国是一个国家</p></body></html>".to_string();
    let mut user_names = HashMap::new();
    user_names.insert(70712u64, "Editor70712".to_string());
    user_names.insert(2345405u64, "Editor2345405".to_string());

    let (action_url, rest_url) =
        spawn_mock_mw(fixture.clone(), parsoid_html, user_names).await;

    let tmp = tempfile::tempdir().unwrap();
    let state = AppState::new(tmp.path().to_path_buf());
    state.install_mw_client(
        &lang,
        MwClientBuilder::for_api_url(&action_url)
            .rest_base_url(&rest_url)
            .between_batches(Duration::from_millis(1))
            .build()
            .unwrap(),
    );
    let base = spawn_server(state.clone()).await;

    // First request: nothing on disk → 200 "still processing"
    // envelope (note: WhoColor's "in progress" is 200, not 408 — see
    // API.md §7 Response (200, in progress)).
    let url = format!("{base}/{lang}/whocolor/v1.0.0-beta/{title}/{rev_id}/?origin=*");
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["success"], Value::Bool(false));
    assert!(
        body["info"].as_str().unwrap_or_default().contains("not currently available"),
        "expected still-processing info, got: {body}"
    );

    // Wait for the background cache-miss to finish.
    wait_for_processing(&state, &lang, page_id).await;

    // Second request: full envelope.
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["success"], Value::Bool(true), "got: {body}");
    assert_eq!(body["page_title"], Value::String(title.clone()));
    assert_eq!(body["rev_id"], rev_id);

    // extended_html should be non-empty and contain at least one
    // editor-token span — 中国 appears in our mock HTML and matches
    // the zh fixture's tokens.
    let html = body["extended_html"].as_str().unwrap();
    assert!(!html.is_empty(), "extended_html should not be empty");
    assert!(
        html.contains("editor-token"),
        "expected at least one editor-token span, got: {html}"
    );

    // tokens is array-of-arrays.
    let tokens = body["tokens"].as_array().unwrap();
    assert!(!tokens.is_empty());
    let first = tokens[0].as_array().unwrap();
    assert_eq!(first.len(), 7, "token tuple has 7 fields");

    // revisions is an object whose keys are rev_ids as strings.
    let revs = body["revisions"].as_object().unwrap();
    assert!(!revs.is_empty());

    // biggest_conflict_score is a number.
    assert!(body["biggest_conflict_score"].is_u64());
}

#[tokio::test]
async fn whocolor_latest_endpoint_uses_last_processed_rev() {
    let Some(fixture) = load_fixture("zh/1686258/64806634") else {
        return;
    };
    let lang = fixture.meta.lang.clone();
    let title = fixture.meta.title.clone();
    let page_id = fixture.meta.page_id;
    let rev_id = fixture.meta.rev_id;

    let parsoid_html =
        "<html><body><p>中国</p></body></html>".to_string();
    let (action_url, rest_url) =
        spawn_mock_mw(fixture.clone(), parsoid_html, HashMap::new()).await;
    let tmp = tempfile::tempdir().unwrap();
    let state = AppState::new(tmp.path().to_path_buf());
    state.install_mw_client(
        &lang,
        MwClientBuilder::for_api_url(&action_url)
            .rest_base_url(&rest_url)
            .between_batches(Duration::from_millis(1))
            .build()
            .unwrap(),
    );
    let base = spawn_server(state.clone()).await;

    // Pre-populate by hitting the rev_id endpoint, then ping the
    // latest endpoint (endpoint 8) and confirm it returns the same
    // rev_id we just processed.
    let _ = reqwest::get(format!(
        "{base}/{lang}/whocolor/v1.0.0-beta/{title}/{rev_id}/"
    ))
    .await
    .unwrap();
    wait_for_processing(&state, &lang, page_id).await;

    let resp = reqwest::get(format!("{base}/{lang}/whocolor/v1.0.0-beta/{title}/"))
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["success"], Value::Bool(true), "got: {body}");
    assert_eq!(body["rev_id"], rev_id);
}

#[tokio::test]
async fn whocolor_rev_id_zero_treats_as_latest() {
    // API.md §7 quirk: titles containing `/` work around the URL
    // ambiguity by passing rev_id=0. Handler should treat that as
    // "latest".
    let Some(fixture) = load_fixture("zh/1686258/64806634") else {
        return;
    };
    let lang = fixture.meta.lang.clone();
    let title = fixture.meta.title.clone();
    let page_id = fixture.meta.page_id;
    let rev_id = fixture.meta.rev_id;

    let parsoid_html =
        "<html><body><p>中国</p></body></html>".to_string();
    let (action_url, rest_url) =
        spawn_mock_mw(fixture.clone(), parsoid_html, HashMap::new()).await;
    let tmp = tempfile::tempdir().unwrap();
    let state = AppState::new(tmp.path().to_path_buf());
    state.install_mw_client(
        &lang,
        MwClientBuilder::for_api_url(&action_url)
            .rest_base_url(&rest_url)
            .between_batches(Duration::from_millis(1))
            .build()
            .unwrap(),
    );
    let base = spawn_server(state.clone()).await;

    // Populate by ingesting via the rev_id endpoint.
    let _ = reqwest::get(format!(
        "{base}/{lang}/whocolor/v1.0.0-beta/{title}/{rev_id}/"
    ))
    .await
    .unwrap();
    wait_for_processing(&state, &lang, page_id).await;

    let resp = reqwest::get(format!(
        "{base}/{lang}/whocolor/v1.0.0-beta/{title}/0/"
    ))
    .await
    .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["success"], Value::Bool(true), "got: {body}");
    assert_eq!(body["rev_id"], rev_id);
}
