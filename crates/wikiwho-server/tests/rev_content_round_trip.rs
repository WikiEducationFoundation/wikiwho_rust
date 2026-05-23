//! End-to-end integration test: write a captured fixture to disk via
//! the storage layer, spin up the server in-process on an ephemeral
//! port, and verify HTTP responses are byte-identical to what
//! `build_rev_content` produces directly.
//!
//! The fixture used (zh/1686258 中国) hits 100% parity on the
//! algorithm side per `notes/2026-05-23-differ-port.md`, so any
//! divergence here is on the server / storage path.

use std::fs;
use std::path::PathBuf;

use serde::Deserialize;
use wikiwho_attribute::pipeline::RevisionInput;
use wikiwho_attribute::response::{ResponseParameters, build_rev_content};
use wikiwho_attribute::structures::Article;
use wikiwho_server::{AppState, router};
use wikiwho_storage::writer::write_article;

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
struct FixtureMeta {
    lang: String,
    title: String,
    page_id: u64,
    rev_id: u64,
}

fn fixture_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.join("parity-fixtures")
}

fn load_fixture(rel: &str) -> Option<(FixtureMeta, Article)> {
    let dir = fixture_root().join(rel);
    let history_path = dir.join("history.jsonl");
    let meta_path = dir.join("meta.json");
    if !history_path.exists() || !meta_path.exists() {
        eprintln!("skipping {rel}: fixture not present");
        return None;
    }
    let meta: FixtureMeta = serde_json::from_str(&fs::read_to_string(&meta_path).unwrap()).unwrap();
    let history_text = fs::read_to_string(&history_path).unwrap();
    let entries: Vec<HistoryEntry> = history_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<HistoryEntry>(l).unwrap())
        .collect();
    let mut article = Article::new(&meta.title);
    article.page_id = Some(meta.page_id);
    for entry in &entries {
        if entry.text_hidden {
            continue;
        }
        article.analyse_revision(RevisionInput {
            rev_id: entry.rev_id,
            timestamp: entry.timestamp.clone(),
            text: entry.text.clone(),
            sha1: entry.sha1.clone(),
            comment: entry.comment.clone(),
            minor: entry.minor,
            user_id: entry.user_id,
            user_name: entry.user_name.clone(),
        });
    }
    Some((meta, article))
}

/// Spawn the server on `127.0.0.1:<ephemeral>` and return (base_url,
/// state). The runtime task owns the listener; the test holds a
/// handle but reqwest closes its connections on drop so the test
/// finishes cleanly without graceful-shutdown plumbing.
async fn spawn_server(state: AppState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn rev_content_by_page_id_round_trip() {
    let Some((meta, article)) = load_fixture("zh/1686258/64806634") else {
        return;
    };
    let target_rev = meta.rev_id;
    let language = meta.lang.clone();
    let page_id = meta.page_id;

    // Persist to a fresh temp directory.
    let tmp = tempfile::tempdir().unwrap();
    write_article(&article, tmp.path(), &language).unwrap();

    // Build the "ground truth" response by calling build_rev_content
    // directly on the in-memory article.
    let expected = build_rev_content(&article, &[target_rev], ResponseParameters::ALL).unwrap();
    let expected_json = serde_json::to_value(&expected).unwrap();

    // Wire up server state + warm the title index (so the lazy
    // path doesn't race against the first request).
    let state = AppState::new(tmp.path().to_path_buf());
    state.refresh_title_index(&language).unwrap();
    let base = spawn_server(state).await;

    // Endpoint 4 — by page_id, latest revision.
    let url = format!(
        "{base}/{lang}/api/v1.0.0-beta/rev_content/page_id/{page_id}/?o_rev_id=true&editor=true&token_id=true&in=true&out=true",
        lang = language,
        page_id = page_id,
    );
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 200, "endpoint 4 status");
    let body: serde_json::Value = resp.json().await.unwrap();

    assert_eq!(body, expected_json, "endpoint 4 body diverged");
}

#[tokio::test]
async fn rev_content_by_title_round_trip() {
    let Some((meta, article)) = load_fixture("zh/1686258/64806634") else {
        return;
    };
    let target_rev = meta.rev_id;
    let language = meta.lang.clone();
    let title = meta.title.clone();

    let tmp = tempfile::tempdir().unwrap();
    write_article(&article, tmp.path(), &language).unwrap();

    let expected = build_rev_content(&article, &[target_rev], ResponseParameters::ALL).unwrap();
    let expected_json = serde_json::to_value(&expected).unwrap();

    let state = AppState::new(tmp.path().to_path_buf());
    state.refresh_title_index(&language).unwrap();
    let base = spawn_server(state).await;

    // Endpoint 2 — by title, latest revision.
    let url = format!(
        "{base}/{lang}/api/v1.0.0-beta/rev_content/{title}/?o_rev_id=true&editor=true&token_id=true&in=true&out=true",
        lang = language,
        title = title,
    );
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 200, "endpoint 2 status");
    let body: serde_json::Value = resp.json().await.unwrap();

    assert_eq!(body, expected_json, "endpoint 2 body diverged");
}

#[tokio::test]
async fn rev_content_by_title_and_rev_round_trip() {
    let Some((meta, article)) = load_fixture("zh/1686258/64806634") else {
        return;
    };
    let target_rev = meta.rev_id;
    let language = meta.lang.clone();
    let title = meta.title.clone();
    let page_id = meta.page_id;

    let tmp = tempfile::tempdir().unwrap();
    write_article(&article, tmp.path(), &language).unwrap();

    let expected = build_rev_content(&article, &[target_rev], ResponseParameters::ALL).unwrap();
    let expected_json = serde_json::to_value(&expected).unwrap();

    let state = AppState::new(tmp.path().to_path_buf());
    state.refresh_title_index(&language).unwrap();
    let base = spawn_server(state).await;

    // Endpoint 3 — by title + specific rev_id.
    let url = format!(
        "{base}/{lang}/api/v1.0.0-beta/rev_content/{title}/{rev_id}/?o_rev_id=true&editor=true&token_id=true&in=true&out=true",
        lang = language,
        title = title,
        rev_id = target_rev,
    );
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 200, "endpoint 3 status");
    let body: serde_json::Value = resp.json().await.unwrap();

    assert_eq!(body, expected_json, "endpoint 3 body diverged");
    // Sanity: confirm we routed through title not page_id by checking the
    // article_title field made it through.
    assert_eq!(body["article_title"], title);
    assert_eq!(body["page_id"], page_id);
}

#[tokio::test]
async fn version_alias_v1_0_0_works() {
    // API.md §"Versioning": both v1.0.0 and v1.0.0-beta must resolve
    // identically.
    let Some((meta, article)) = load_fixture("zh/1686258/64806634") else {
        return;
    };
    let language = meta.lang.clone();
    let page_id = meta.page_id;

    let tmp = tempfile::tempdir().unwrap();
    write_article(&article, tmp.path(), &language).unwrap();
    let state = AppState::new(tmp.path().to_path_buf());
    let base = spawn_server(state).await;

    let url = format!(
        "{base}/{lang}/api/v1.0.0/rev_content/page_id/{page_id}/",
        lang = language,
        page_id = page_id,
    );
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 200, "v1.0.0 should resolve identically to v1.0.0-beta");
}

#[tokio::test]
async fn latest_rev_content_alias_works() {
    // Endpoint 5: `/latest_rev_content/{title}/` is an alias of (2).
    let Some((meta, article)) = load_fixture("zh/1686258/64806634") else {
        return;
    };
    let language = meta.lang.clone();
    let title = meta.title.clone();

    let tmp = tempfile::tempdir().unwrap();
    write_article(&article, tmp.path(), &language).unwrap();
    let state = AppState::new(tmp.path().to_path_buf());
    state.refresh_title_index(&language).unwrap();
    let base = spawn_server(state).await;

    let url = format!(
        "{base}/{lang}/api/v1.0.0-beta/latest_rev_content/{title}/",
        lang = language,
        title = title,
    );
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 200, "latest_rev_content alias should match rev_content");
}

#[tokio::test]
async fn missing_article_returns_still_processing() {
    let tmp = tempfile::tempdir().unwrap();
    let state = AppState::new(tmp.path().to_path_buf());
    let base = spawn_server(state).await;

    // No fixture written; the page_id is unknown.
    let url = format!("{base}/en/api/v1.0.0-beta/rev_content/page_id/99999999/");
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 408, "unprocessed article should return 408");

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body.get("Info").is_some(),
        "envelope must contain Info field: {body}"
    );
}

#[tokio::test]
async fn missing_title_returns_still_processing() {
    let tmp = tempfile::tempdir().unwrap();
    let state = AppState::new(tmp.path().to_path_buf());
    let base = spawn_server(state).await;

    let url = format!("{base}/en/api/v1.0.0-beta/rev_content/Some_Article/");
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 408, "missing-title should mirror missing-page envelope");
}

#[tokio::test]
async fn rev_content_by_rev_id_round_trip() {
    // Endpoint 1 — Impact Visualizer's main entry point.
    // The rev_id is resolved through `rev_id_index.bin` to a page_id,
    // then rendered the same way endpoints 3/4/6 do.
    let Some((meta, article)) = load_fixture("zh/1686258/64806634") else {
        return;
    };
    let target_rev = meta.rev_id;
    let language = meta.lang.clone();
    let page_id = meta.page_id;

    let tmp = tempfile::tempdir().unwrap();
    write_article(&article, tmp.path(), &language).unwrap();

    let expected = build_rev_content(&article, &[target_rev], ResponseParameters::ALL).unwrap();
    let expected_json = serde_json::to_value(&expected).unwrap();

    let state = AppState::new(tmp.path().to_path_buf());
    state.refresh_rev_id_index(&language).unwrap();
    let base = spawn_server(state).await;

    let url = format!(
        "{base}/{lang}/api/v1.0.0-beta/rev_content/rev_id/{rev_id}/?o_rev_id=true&editor=true&token_id=true&in=true&out=true",
        lang = language,
        rev_id = target_rev,
    );
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 200, "endpoint 1 should resolve via rev_id_index.bin");
    let body: serde_json::Value = resp.json().await.unwrap();

    assert_eq!(body, expected_json, "endpoint 1 body diverged");
    assert_eq!(body["page_id"], page_id);
}

#[tokio::test]
async fn rev_content_by_rev_id_unknown_returns_still_processing() {
    // Same fixture on disk; ask for a rev_id that doesn't exist anywhere
    // in the index. Should hit the "still processing" placeholder, not
    // a 500 — Impact Visualizer's client treats 408 as "skip this
    // article" and tries again later.
    let Some((meta, article)) = load_fixture("zh/1686258/64806634") else {
        return;
    };
    let language = meta.lang.clone();

    let tmp = tempfile::tempdir().unwrap();
    write_article(&article, tmp.path(), &language).unwrap();

    let state = AppState::new(tmp.path().to_path_buf());
    state.refresh_rev_id_index(&language).unwrap();
    let base = spawn_server(state).await;

    let url = format!(
        "{base}/{lang}/api/v1.0.0-beta/rev_content/rev_id/9000000000/",
        lang = language,
    );
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 408, "unknown rev_id should mirror missing-article envelope");

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body.get("Info").is_some(),
        "envelope must contain Info field: {body}"
    );
}

#[tokio::test]
async fn rev_content_by_rev_id_uses_lazy_index_load() {
    // No explicit refresh_rev_id_index call: the first request must
    // build the cache lazily, just like the title-index path does.
    let Some((meta, article)) = load_fixture("zh/1686258/64806634") else {
        return;
    };
    let target_rev = meta.rev_id;
    let language = meta.lang.clone();

    let tmp = tempfile::tempdir().unwrap();
    write_article(&article, tmp.path(), &language).unwrap();

    let state = AppState::new(tmp.path().to_path_buf());
    // NOTE: no state.refresh_rev_id_index(...) here.
    let base = spawn_server(state).await;

    let url = format!(
        "{base}/{lang}/api/v1.0.0-beta/rev_content/rev_id/{rev_id}/",
        lang = language,
        rev_id = target_rev,
    );
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 200, "lazy index load should serve endpoint 1");
}

#[tokio::test]
async fn rev_content_token_field_filtering() {
    let Some((meta, article)) = load_fixture("zh/1686258/64806634") else {
        return;
    };
    let language = meta.lang.clone();
    let page_id = meta.page_id;

    let tmp = tempfile::tempdir().unwrap();
    write_article(&article, tmp.path(), &language).unwrap();
    let state = AppState::new(tmp.path().to_path_buf());
    let base = spawn_server(state).await;

    // No query params — only `str` should be present on each token.
    let url = format!(
        "{base}/{lang}/api/v1.0.0-beta/rev_content/page_id/{page_id}/",
        lang = language,
        page_id = page_id,
    );
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let token = &body["revisions"][0]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()["tokens"][0];
    assert!(token.get("str").is_some(), "str always present");
    for omitted in ["o_rev_id", "editor", "token_id", "in", "out"] {
        assert!(
            token.get(omitted).is_none(),
            "field {omitted} should be omitted when not requested"
        );
    }
}
