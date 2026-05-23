//! End-to-end round-trip test against captured-history fixtures.
//!
//! For each fixture: replay every revision through the algorithm,
//! persist to disk via `wikiwho-storage`, reload, and verify that
//! `build_rev_content` produces byte-identical JSON before and after.
//!
//! This is the load-bearing test of the storage layer per
//! `CLAUDE.md`: it proves the on-disk format preserves all state
//! that downstream consumers see in the wire response.

use std::fs;
use std::path::PathBuf;

use serde::Deserialize;
use wikiwho_attribute::pipeline::RevisionInput;
use wikiwho_attribute::response::{ResponseParameters, build_rev_content};
use wikiwho_attribute::structures::Article;
use wikiwho_storage::reader::SnapshotReader;
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
    // The test runs from the crate dir; parity-fixtures lives at the
    // workspace root.
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/wikiwho-storage -> crates
    p.pop(); // crates -> workspace root
    p.join("parity-fixtures")
}

fn load_fixture(rel: &str) -> Option<(FixtureMeta, Article)> {
    let dir = fixture_root().join(rel);
    let history_path = dir.join("history.jsonl");
    let meta_path = dir.join("meta.json");
    if !history_path.exists() || !meta_path.exists() {
        eprintln!(
            "skipping {rel}: fixture not present (history.jsonl or meta.json missing)"
        );
        return None;
    }
    let meta: FixtureMeta = serde_json::from_str(&fs::read_to_string(&meta_path).unwrap())
        .expect("meta.json shape");
    let history_text = fs::read_to_string(&history_path).unwrap();
    let entries: Vec<HistoryEntry> = history_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<HistoryEntry>(l).expect("history line shape"))
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

/// Persist + reload + verify wire format equals.
fn run_round_trip(rel: &str) {
    let Some((meta, article)) = load_fixture(rel) else {
        return;
    };
    let target_rev = meta.rev_id;
    let language = meta.lang.clone();
    let page_id = meta.page_id;

    let before = build_rev_content(&article, &[target_rev], ResponseParameters::ALL)
        .expect("in-memory build");
    let before_json = serde_json::to_string(&before).unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let written_dir = write_article(&article, tmp.path(), &language).expect("write_article");
    // The shard layout must match what SnapshotReader::open expects.
    let opened = SnapshotReader::open(tmp.path(), &language, page_id).expect("open by page_id");
    assert_eq!(opened.dir, written_dir, "writer and reader disagree on path");

    let after = build_rev_content(&opened.article, &[target_rev], ResponseParameters::ALL)
        .expect("storage-backed build");
    let after_json = serde_json::to_string(&after).unwrap();

    assert_eq!(
        before_json, after_json,
        "wire format diverged for fixture {rel} target rev {target_rev}"
    );
}

#[test]
fn round_trip_zh_1686258() {
    // 中国 — 7 revs, hits 100 % parity per notes/2026-05-23-differ-port.md.
    run_round_trip("zh/1686258/64806634");
}

#[test]
fn round_trip_en_79023819() {
    // Israel-Hamas war stub — 2 revs.
    run_round_trip("en/79023819/1277418181");
}

#[test]
fn round_trip_simple_27263() {
    // simple Wikipedia — 3.8k revs, exercises high-vandalism path.
    // Skips silently if not captured locally.
    run_round_trip("simple/27263/10855732");
}

#[test]
fn round_trip_en_24544_photosynthesis() {
    // Photosynthesis — 5.5k revs, exercises full-history scale.
    run_round_trip("en/24544/1354638187");
}

/// Verify that ALL revisions in the fixture (not just the target)
/// round-trip — catches any per-revision encoding bugs that aren't
/// visible from the final rev alone.
#[test]
fn round_trip_every_revision_zh() {
    let Some((meta, article)) = load_fixture("zh/1686258/64806634") else {
        return;
    };
    let language = meta.lang.clone();
    let page_id = meta.page_id;
    let revs: Vec<u64> = article.ordered_revisions.clone();

    let tmp = tempfile::tempdir().unwrap();
    write_article(&article, tmp.path(), &language).unwrap();
    let opened = SnapshotReader::open(tmp.path(), &language, page_id).unwrap();

    for rev_id in revs {
        let before = build_rev_content(&article, &[rev_id], ResponseParameters::ALL).unwrap();
        let after =
            build_rev_content(&opened.article, &[rev_id], ResponseParameters::ALL).unwrap();
        assert_eq!(
            serde_json::to_string(&before).unwrap(),
            serde_json::to_string(&after).unwrap(),
            "diverged on rev {rev_id}"
        );
    }
}
