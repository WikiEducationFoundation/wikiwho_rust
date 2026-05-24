//! End-to-end test of `apply_event` against a captured-history fixture.
//!
//! Pattern:
//! 1. Load the fixture's history.jsonl.
//! 2. Replay all but the last N revisions through the algorithm and
//!    persist via `write_article` — simulating "what's on disk before
//!    the SSE event arrives."
//! 3. Spin up a tiny axum mock that pretends to be the MW Action API
//!    and returns the remaining revisions when queried.
//! 4. Synthesize a `PageEdit` for the last rev and call `apply_event`.
//! 5. Reload the snapshot and verify `build_rev_content` matches a
//!    full in-memory replay — i.e. the loaded-applied-saved state
//!    produces the same wire format as if we'd replayed the whole
//!    history from scratch.
//!
//! This is the load-bearing test that the ingest scaffold actually
//! ports state across the network → disk → algorithm boundary
//! correctly.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use wikiwho_attribute::pipeline::RevisionInput;
use wikiwho_attribute::response::{ResponseParameters, build_rev_content};
use wikiwho_attribute::structures::Article;
use wikiwho_ingest::{ApplyOutcome, PageEdit, apply_event};
use wikiwho_mwclient::MwClientBuilder;
use wikiwho_storage::reader::SnapshotReader;
use wikiwho_storage::writer::write_article;

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

#[derive(Debug, Deserialize)]
struct FixtureMeta {
    lang: String,
    title: String,
    page_id: u64,
}

fn fixture_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/wikiwho-ingest -> crates
    p.pop(); // crates -> workspace root
    p.join("parity-fixtures")
}

fn load_fixture(rel: &str) -> Option<(FixtureMeta, Vec<HistoryEntry>)> {
    let dir = fixture_root().join(rel);
    let history_path = dir.join("history.jsonl");
    let meta_path = dir.join("meta.json");
    if !history_path.exists() || !meta_path.exists() {
        return None;
    }
    let meta: FixtureMeta = serde_json::from_str(&std::fs::read_to_string(&meta_path).ok()?).ok()?;
    let history_text = std::fs::read_to_string(&history_path).ok()?;
    let entries: Vec<HistoryEntry> = history_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<HistoryEntry>(l).expect("history line shape"))
        .collect();
    Some((meta, entries))
}

fn replay(meta: &FixtureMeta, entries: &[HistoryEntry]) -> Article {
    let mut article = Article::new(&meta.title);
    article.page_id = Some(meta.page_id);
    for entry in entries {
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
    article
}

/// Mock state — a slice of the captured history keyed by page_id.
#[derive(Clone)]
struct MwMock {
    /// page_id -> list of (rev_id, raw JSON object in fv=2 shape)
    revisions: Arc<HashMap<u64, Vec<Value>>>,
}

async fn query_handler(
    State(state): State<MwMock>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let page_id: u64 = params
        .get("pageids")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let rvendid: u64 = params
        .get("rvendid")
        .and_then(|s| s.parse().ok())
        .unwrap_or(u64::MAX);
    let Some(revs) = state.revisions.get(&page_id) else {
        return (
            StatusCode::OK,
            Json(json!({
                "batchcomplete": true,
                "query": {
                    "pages": [{"pageid": page_id, "missing": true}]
                }
            })),
        );
    };
    // Action API rvdir=newer is oldest-first; mock returns everything
    // up to rvendid (inclusive) in that order.
    let filtered: Vec<&Value> = revs
        .iter()
        .filter(|r| {
            r.get("revid")
                .and_then(|v| v.as_u64())
                .map(|id| id <= rvendid)
                .unwrap_or(false)
        })
        .collect();
    (
        StatusCode::OK,
        Json(json!({
            "batchcomplete": true,
            "query": {
                "pages": [{
                    "pageid": page_id,
                    "ns": 0,
                    "title": "T",
                    "revisions": filtered,
                }]
            }
        })),
    )
}

fn entry_to_mw_revision(e: &HistoryEntry) -> Value {
    // formatversion=2 shape — see parse_revision in mwclient.
    let mut rev = json!({
        "revid": e.rev_id,
        "parentid": 0,
        "timestamp": e.timestamp,
        "minor": e.minor,
        "slots": { "main": { "content": e.text } },
    });
    if let Some(s) = &e.sha1 {
        rev["sha1"] = json!(s);
    }
    if let Some(c) = &e.comment {
        rev["comment"] = json!(c);
    }
    if let Some(uid) = e.user_id {
        rev["userid"] = json!(uid);
    }
    if let Some(un) = &e.user_name {
        rev["user"] = json!(un);
    }
    if e.text_hidden {
        rev["slots"]["main"]["texthidden"] = json!(true);
    }
    rev
}

async fn spawn_mw_mock(revisions: HashMap<u64, Vec<Value>>) -> String {
    let state = MwMock {
        revisions: Arc::new(revisions),
    };
    let app = Router::new()
        .route("/w/api.php", get(query_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/w/api.php")
}

async fn run_for_fixture(rel: &str, split_at: usize) {
    let Some((meta, entries)) = load_fixture(rel) else {
        eprintln!("skipping {rel}: fixture not on disk");
        return;
    };
    assert!(split_at < entries.len(), "split must leave at least 1 unseen rev");

    // Snapshot: replay everything for the reference.
    let full = replay(&meta, &entries);

    // Persist after first `split_at` revs.
    let partial = replay(&meta, &entries[..split_at]);
    let tmp = tempfile::tempdir().unwrap();
    write_article(&partial, tmp.path(), &meta.lang).unwrap();

    // Build the MW mock with the remaining revisions in fv=2 shape.
    let remaining: Vec<Value> = entries
        .iter()
        .skip(split_at)
        .filter(|e| !e.text_hidden)
        .map(entry_to_mw_revision)
        .collect();
    let mut mock_db = HashMap::new();
    mock_db.insert(meta.page_id, remaining.clone());
    let api_url = spawn_mw_mock(mock_db).await;

    let client = MwClientBuilder::for_api_url(api_url)
        .between_batches(Duration::from_millis(1))
        .build()
        .unwrap();

    // Synthesize the SSE event for the last rev.
    let last_entry = entries.iter().rfind(|e| !e.text_hidden).unwrap();
    let prev_entry_rev_id = entries
        .iter()
        .rev()
        .skip(1)
        .find(|e| !e.text_hidden)
        .map(|e| e.rev_id)
        .unwrap_or(0);
    let event = PageEdit {
        language: meta.lang.clone(),
        wiki: format!("{}wiki", meta.lang),
        page_id: meta.page_id,
        rev_id: last_entry.rev_id,
        parent_rev_id: prev_entry_rev_id,
        title: meta.title.clone(),
        sse_id: None,
    };

    let outcome = apply_event(tmp.path(), &client, &event).await.unwrap();
    match outcome {
        ApplyOutcome::Applied { applied_revs } => {
            assert!(
                applied_revs >= 1,
                "expected at least 1 applied rev, got {applied_revs}"
            );
        }
        other => panic!("expected Applied, got {other:?}"),
    }

    // Reload and compare wire format on the target rev.
    let reader = SnapshotReader::open(tmp.path(), &meta.lang, meta.page_id).unwrap();
    let applied = reader.article;

    let want = build_rev_content(&full, &[last_entry.rev_id], ResponseParameters::ALL).unwrap();
    let got = build_rev_content(&applied, &[last_entry.rev_id], ResponseParameters::ALL).unwrap();
    assert_eq!(
        serde_json::to_string(&want).unwrap(),
        serde_json::to_string(&got).unwrap(),
        "wire-format diverged for {rel} (split_at={split_at})"
    );
    // Structural too — guards against state that survives the wire
    // format check but breaks on the next revision.
    assert_eq!(applied.tokens.len(), full.tokens.len());
    assert_eq!(applied.paragraphs_ht.len(), full.paragraphs_ht.len());
    assert_eq!(applied.sentences_ht.len(), full.sentences_ht.len());
    assert_eq!(applied.ordered_revisions, full.ordered_revisions);
}

#[tokio::test]
async fn apply_one_event_zh() {
    // 7-rev fixture; persist after rev 6, apply the 7th via SSE.
    run_for_fixture("zh/1686258/64806634", 6).await;
}

#[tokio::test]
async fn apply_gap_window_zh() {
    // Persist after rev 3, simulate an SSE event that only knows about
    // rev 7. The apply path should fetch revs 4-7 inclusive and apply
    // them all in order.
    run_for_fixture("zh/1686258/64806634", 3).await;
}
