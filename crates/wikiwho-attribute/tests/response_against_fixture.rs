//! End-to-end test of `response::build_rev_content` against a captured
//! `rev_content.json` fixture.
//!
//! Pipeline: replay the full `history.jsonl` through
//! `Article::analyse_revision`, build the wire-format response with
//! `ResponseParameters::ALL`, then structurally compare against the
//! `python_replay.json` ground truth that ships alongside each fixture.
//!
//! Why `python_replay.json`, not `rev_content.json`? Both contain the
//! same shape (article_title, page_id, tokens with the full param
//! set), but `python_replay.json` is a fresh Python re-run on the same
//! `history.jsonl` — it doesn't carry historical-state drift from the
//! production cache. See `notes/decisions-needed.md` for the rationale.
//!
//! NOTE: `python_replay.json` is a *debug* shape (final_tokens only,
//! no envelope), not the wire format. So we structurally diff the
//! token list, not the full envelope. The wire-format envelope is
//! covered by `response.rs`'s unit tests.

use std::fs;
use std::path::PathBuf;

use serde_json::Value;

use wikiwho_attribute::pipeline::RevisionInput;
use wikiwho_attribute::response::{ResponseParameters, build_rev_content};
use wikiwho_attribute::structures::Article;

#[derive(serde::Deserialize)]
struct HistoryRow {
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

#[derive(serde::Deserialize)]
struct PythonReplay {
    title: String,
    page_id: u64,
    target_rev_id: u64,
    final_tokens: Vec<Value>,
}

fn replay_fixture(fixture_dir: &str) -> Option<(Article, PythonReplay)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(fixture_dir);
    let history_path = dir.join("history.jsonl");
    let replay_path = dir.join("python_replay.json");
    if !history_path.exists() || !replay_path.exists() {
        return None;
    }
    let history_blob = fs::read_to_string(&history_path).ok()?;
    let replay: PythonReplay =
        serde_json::from_str(&fs::read_to_string(&replay_path).ok()?).ok()?;
    let mut article = Article::new(&replay.title);
    article.page_id = Some(replay.page_id);

    for (i, line) in history_blob.lines().enumerate() {
        let row: HistoryRow = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("decode line {i} in {fixture_dir}: {e}"));
        if row.text_hidden {
            continue;
        }
        article.analyse_revision(RevisionInput {
            rev_id: row.rev_id,
            timestamp: row.timestamp,
            text: row.text,
            sha1: row.sha1,
            comment: row.comment,
            minor: row.minor,
            user_id: row.user_id,
            user_name: row.user_name,
        });
    }
    Some((article, replay))
}

fn assert_tokens_match(ours: &[Value], theirs: &[Value], label: &str) {
    assert_eq!(
        ours.len(),
        theirs.len(),
        "{label}: token count differs (rust={}, python={})",
        ours.len(),
        theirs.len()
    );
    for (i, (a, b)) in ours.iter().zip(theirs.iter()).enumerate() {
        for field in &["str", "o_rev_id", "token_id", "in", "out"] {
            let av = a.get(field);
            let bv = b.get(field);
            assert_eq!(
                av, bv,
                "{label}: token[{i}].{field} differs (rust={av:?}, python={bv:?})"
            );
        }
    }
}

#[test]
fn zh_zhongguo_wire_format_matches_python_replay() {
    let Some((article, replay)) =
        replay_fixture("parity-fixtures/zh/1686258/64806634")
    else {
        eprintln!("skip: fixture missing");
        return;
    };
    let resp = build_rev_content(&article, &[replay.target_rev_id], ResponseParameters::ALL)
        .expect("build response");
    let envelope = serde_json::to_value(&resp).unwrap();
    let our_tokens = envelope["revisions"][0]
        [replay.target_rev_id.to_string()]["tokens"]
        .as_array()
        .expect("tokens array")
        .clone();
    assert_tokens_match(&our_tokens, &replay.final_tokens, "zh/1686258");
}

#[test]
fn israel_hamas_wire_format_matches_python_replay() {
    let Some((article, replay)) =
        replay_fixture("parity-fixtures/en/79023819/1277418181")
    else {
        eprintln!("skip: fixture missing");
        return;
    };
    let resp = build_rev_content(&article, &[replay.target_rev_id], ResponseParameters::ALL)
        .expect("build response");
    let envelope = serde_json::to_value(&resp).unwrap();
    let our_tokens = envelope["revisions"][0]
        [replay.target_rev_id.to_string()]["tokens"]
        .as_array()
        .expect("tokens array")
        .clone();
    assert_tokens_match(&our_tokens, &replay.final_tokens, "en/79023819");
}
