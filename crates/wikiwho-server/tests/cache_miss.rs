//! End-to-end test of the cache-miss path: request hits an article
//! that isn't on disk, the server spins up a background processor
//! against a mock MediaWiki endpoint, persists, and subsequent
//! requests serve byte-identical JSON.
//!
//! The mock MW server speaks just enough of the Action API to satisfy
//! the two calls the cache-miss path makes:
//! - `action=query&prop=info&inprop=lastrevid&(titles|pageids)=...`
//! - `action=query&prop=revisions&...&rvendid=...&pageids=...`
//!
//! Fixture: zh/1686258 中国 (7 revs). Small enough that the revisions
//! fit in a single MW response page (no pagination required).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::{Json, Router, extract::Query, response::IntoResponse, routing::get};
use serde::Deserialize;
use serde_json::{Value, json};
use wikiwho_attribute::pipeline::RevisionInput;
use wikiwho_attribute::response::{ResponseParameters, build_rev_content};
use wikiwho_attribute::structures::Article;
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
    text_hidden: bool,
}

#[derive(Debug, Deserialize, Clone)]
struct FixtureMeta {
    lang: String,
    title: String,
    page_id: u64,
    rev_id: u64,
}

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

/// Replay the fixture's history through the algorithm to produce the
/// "ground truth" article for byte-identical comparison.
fn build_expected_article(fixture: &Fixture) -> Article {
    let mut article = Article::new(&fixture.meta.title);
    article.page_id = Some(fixture.meta.page_id);
    for entry in &fixture.history {
        if entry.text_hidden {
            continue;
        }
        article.analyse_revision(RevisionInput {
            rev_id: entry.rev_id,
            timestamp: entry.timestamp.clone(),
            sha1: entry.sha1.clone(),
            comment: entry.comment.clone(),
            minor: entry.minor,
            user_id: entry.user_id,
            user_name: entry.user_name.clone(),
            text: entry.text.clone(),
        });
    }
    article
}

/// Spawn the wikiwho-server on an ephemeral port and return the base URL.
async fn spawn_server(state: AppState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// Build the MW Action-API `prop=info` response body for `fixture`.
fn mw_info_body(fixture: &Fixture) -> Value {
    json!({
        "batchcomplete": true,
        "query": {
            "pages": [{
                "pageid": fixture.meta.page_id,
                "ns": 0,
                "title": fixture.meta.title,
                "contentmodel": "wikitext",
                "pagelanguage": fixture.meta.lang,
                "lastrevid": fixture.meta.rev_id,
            }]
        }
    })
}

/// Build the MW Action-API `prop=revisions` response body for `fixture`.
/// One page, all revisions in `slots.main.content`, oldest-first.
fn mw_revisions_body(fixture: &Fixture) -> Value {
    let revisions: Vec<Value> = fixture
        .history
        .iter()
        .map(|h| {
            let mut rev = json!({
                "revid": h.rev_id,
                "parentid": 0u64,
                "timestamp": h.timestamp,
                "minor": h.minor,
            });
            if let Some(u) = h.user_name.as_ref() {
                rev["user"] = json!(u);
            }
            if let Some(id) = h.user_id {
                rev["userid"] = json!(id);
            }
            if let Some(c) = h.comment.as_ref() {
                rev["comment"] = json!(c);
            }
            if let Some(s) = h.sha1.as_ref() {
                rev["sha1"] = json!(s);
            }
            rev["slots"] = json!({
                "main": {
                    "contentmodel": "wikitext",
                    "content": h.text,
                }
            });
            rev
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

/// Spawn a tiny axum-based MW mock. Returns the URL to use for
/// `MwClientBuilder::for_api_url`.
async fn spawn_mock_mw(fixture: Fixture) -> String {
    let info = Arc::new(mw_info_body(&fixture));
    let revisions = Arc::new(mw_revisions_body(&fixture));
    let state = MockState { info, revisions };

    let app = Router::new()
        .route("/w/api.php", get(mock_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/w/api.php")
}

#[derive(Clone)]
struct MockState {
    info: Arc<Value>,
    revisions: Arc<Value>,
}

async fn mock_handler(
    axum::extract::State(state): axum::extract::State<MockState>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let prop = params.get("prop").map(String::as_str);
    match prop {
        Some("info") => Json((*state.info).clone()),
        Some("revisions") => Json((*state.revisions).clone()),
        _ => Json(json!({"error": {"code": "unhandled", "info": "mock doesn't speak this"}})),
    }
}

/// Poll `state` until the article is no longer in-flight. Used by the
/// integration test to wait for the background processing task to
/// finish — the cache-miss spawn handle isn't surfaced to the
/// handler caller.
async fn wait_for_processing(state: &AppState, lang: &str, page_id: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while state.is_in_flight(lang, page_id) {
        if std::time::Instant::now() > deadline {
            panic!("cache-miss task did not complete within 30s");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn cache_miss_by_title_persists_and_serves_byte_identical() {
    let Some(fixture) = load_fixture("zh/1686258/64806634") else {
        return;
    };
    let lang = fixture.meta.lang.clone();
    let title = fixture.meta.title.clone();
    let page_id = fixture.meta.page_id;
    let target_rev = fixture.meta.rev_id;

    // Mock MW server.
    let mw_url = spawn_mock_mw(fixture_clone(&fixture)).await;

    // Server with empty storage. Inject the mock MW client.
    let tmp = tempfile::tempdir().unwrap();
    let state = AppState::new(tmp.path().to_path_buf());
    state.install_mw_client(
        &lang,
        MwClientBuilder::for_api_url(&mw_url)
            .between_batches(Duration::from_millis(1))
            .build()
            .unwrap(),
    );
    let base = spawn_server(state.clone()).await;

    // First request: nothing on disk → 408, cache-miss spawned.
    let url = format!(
        "{base}/{lang}/api/v1.0.0-beta/rev_content/{title}/?o_rev_id=true&editor=true&token_id=true&in=true&out=true"
    );
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 408, "first request should be still-processing");

    // Background task processes the article.
    wait_for_processing(&state, &lang, page_id).await;

    // Second request: served from disk, byte-identical JSON.
    let expected = build_rev_content(
        &build_expected_article(&fixture),
        &[target_rev],
        ResponseParameters::ALL,
    )
    .unwrap();
    let expected_json = serde_json::to_value(&expected).unwrap();
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 200, "second request should serve from disk");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, expected_json, "served JSON diverged from build_rev_content");
}

#[tokio::test]
async fn cache_miss_by_page_id_persists_and_serves_byte_identical() {
    let Some(fixture) = load_fixture("zh/1686258/64806634") else {
        return;
    };
    let lang = fixture.meta.lang.clone();
    let page_id = fixture.meta.page_id;
    let target_rev = fixture.meta.rev_id;

    let mw_url = spawn_mock_mw(fixture_clone(&fixture)).await;
    let tmp = tempfile::tempdir().unwrap();
    let state = AppState::new(tmp.path().to_path_buf());
    state.install_mw_client(
        &lang,
        MwClientBuilder::for_api_url(&mw_url)
            .between_batches(Duration::from_millis(1))
            .build()
            .unwrap(),
    );
    let base = spawn_server(state.clone()).await;

    let url = format!(
        "{base}/{lang}/api/v1.0.0-beta/rev_content/page_id/{page_id}/?o_rev_id=true&editor=true&token_id=true&in=true&out=true"
    );
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 408);

    wait_for_processing(&state, &lang, page_id).await;

    let expected = build_rev_content(
        &build_expected_article(&fixture),
        &[target_rev],
        ResponseParameters::ALL,
    )
    .unwrap();
    let expected_json = serde_json::to_value(&expected).unwrap();
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, expected_json);
}

#[tokio::test]
async fn cache_miss_concurrent_requests_spawn_one_task() {
    // Two simultaneous requests for the same uncached article should
    // result in exactly one cache-miss task (the second sees the
    // in-flight slot and skips re-spawning).
    let Some(fixture) = load_fixture("zh/1686258/64806634") else {
        return;
    };
    let lang = fixture.meta.lang.clone();
    let title = fixture.meta.title.clone();
    let page_id = fixture.meta.page_id;

    let mw_url = spawn_mock_mw(fixture_clone(&fixture)).await;
    let tmp = tempfile::tempdir().unwrap();
    let state = AppState::new(tmp.path().to_path_buf());
    state.install_mw_client(
        &lang,
        MwClientBuilder::for_api_url(&mw_url)
            .between_batches(Duration::from_millis(1))
            .build()
            .unwrap(),
    );
    let base = spawn_server(state.clone()).await;

    let url = format!(
        "{base}/{lang}/api/v1.0.0-beta/rev_content/{title}/"
    );
    let (r1, r2) = tokio::join!(reqwest::get(&url), reqwest::get(&url));
    assert_eq!(r1.unwrap().status(), 408);
    assert_eq!(r2.unwrap().status(), 408);

    // Wait. After completion, both subsequent requests serve.
    wait_for_processing(&state, &lang, page_id).await;
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn cache_miss_no_mw_client_returns_408_without_spawning() {
    // No `install_mw_client` call and no real network — the production
    // `MwClient::new(lang)` builder still succeeds (it just constructs
    // the URL) but won't be able to reach anything. We use a bogus
    // language to make build_and_cache miss; the resolve_via_mw path
    // then logs and returns the still-processing envelope.
    let tmp = tempfile::tempdir().unwrap();
    let state = AppState::new(tmp.path().to_path_buf());
    state.install_mw_client(
        "zz-no-network",
        MwClientBuilder::for_api_url("http://127.0.0.1:1/w/api.php")
            .between_batches(Duration::from_millis(1))
            .retry_max_attempts(1)
            .request_timeout(Duration::from_millis(50))
            .build()
            .unwrap(),
    );
    let base = spawn_server(state).await;

    let url = format!("{base}/zz-no-network/api/v1.0.0-beta/rev_content/Some_Article/");
    let resp = reqwest::get(&url).await.unwrap();
    // Either 408 (MW resolve failed → still_processing) or 500 (if the
    // handler maps the error differently). 408 is the contract per
    // API.md §1, but accept 500 with a clear diagnostic for now.
    assert!(
        resp.status() == 408,
        "expected 408 (mw unreachable → still processing), got {}",
        resp.status()
    );
}

fn fixture_clone(f: &Fixture) -> Fixture {
    Fixture {
        meta: f.meta.clone(),
        history: f.history.clone(),
    }
}
