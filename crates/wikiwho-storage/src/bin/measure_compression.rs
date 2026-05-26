//! `measure_compression` — replay a history.jsonl fixture through the
//! algorithm, write it out via the current `write_article`, then report
//! raw size + zstd-3 size + ratio for every file in the article
//! directory.
//!
//! This is a one-shot measurement tool. It's how we decide which files
//! warrant being compressed-on-disk. See
//! `notes/2026-05-25-storage-compression.md` for the numbers it
//! produced.
//!
//! Usage:
//!   measure_compression <fixture-dir>         # e.g. parity-fixtures/en/24544/<rev_id>
//!   measure_compression <fixture-dir> 1000    # cap revisions for a quick check

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use wikiwho_attribute::pipeline::RevisionInput;
use wikiwho_attribute::structures::Article;
use wikiwho_storage::writer::write_article;

#[derive(Debug, Deserialize)]
struct Meta {
    lang: String,
    title: String,
    page_id: u64,
}

#[derive(Debug, Deserialize)]
struct HistoryEntry {
    rev_id: u64,
    timestamp: String,
    #[serde(default)]
    sha1: Option<String>,
    #[serde(default)]
    comment: Option<String>,
    #[serde(default)]
    minor: bool,
    #[serde(default)]
    user_id: Option<u64>,
    #[serde(default)]
    user_name: Option<String>,
    #[serde(default)]
    text: String,
    #[serde(default)]
    text_hidden: bool,
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let fixture: PathBuf = args
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("usage: measure_compression <fixture-dir> [max-revs]"))?;
    let max_revs: Option<usize> = args.next().map(|s| s.parse()).transpose()?;

    let meta_path = fixture.join("meta.json");
    let history_path = fixture.join("history.jsonl");
    if !meta_path.is_file() || !history_path.is_file() {
        bail!(
            "{} is missing meta.json or history.jsonl",
            fixture.display()
        );
    }
    let meta: Meta = serde_json::from_str(&fs::read_to_string(&meta_path)?)?;

    let history = fs::read_to_string(&history_path)
        .with_context(|| format!("reading {}", history_path.display()))?;
    let mut entries: Vec<HistoryEntry> = history
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    if let Some(cap) = max_revs {
        entries.truncate(cap);
    }

    eprintln!(
        "replaying {} rev(s) of {}/{} ({} page_id={})…",
        entries.len(),
        meta.lang,
        meta.title,
        fixture.display(),
        meta.page_id
    );
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

    let tmp = tempfile::tempdir()?;
    let dir = write_article(&article, tmp.path(), &meta.lang)?;
    eprintln!("wrote {}", dir.display());

    // Read every file in the article dir, report (uncompressed,
    // on-disk) sizes. Every `.bin` file is zstd-compressed on disk
    // (see `writer::ZSTD_LEVEL`); the "uncompressed" column comes from
    // `zstd::decode_all`, the "on_disk" column is the actual on-disk
    // size, and "ratio" is how much disk we save vs. the raw payload.
    // `meta.json` is plaintext and reports the same value in both
    // columns.
    let mut entries: Vec<_> = fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    entries.sort();

    println!();
    println!(
        "{:<18}  {:>14}  {:>14}  {:>8}",
        "file", "uncompressed", "on_disk", "ratio"
    );
    println!("{}", "-".repeat(60));
    let mut total_uncompressed = 0u64;
    let mut total_on_disk = 0u64;
    for path in &entries {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let on_disk = fs::read(path)?;
        let is_bin = name.ends_with(".bin");
        let uncompressed = if is_bin {
            zstd::decode_all(&on_disk[..])?.len()
        } else {
            on_disk.len()
        };
        let ratio = if on_disk.is_empty() {
            0.0
        } else {
            uncompressed as f64 / on_disk.len() as f64
        };
        println!(
            "{:<18}  {:>14}  {:>14}  {:>7.2}x",
            name,
            uncompressed,
            on_disk.len(),
            ratio
        );
        total_uncompressed += uncompressed as u64;
        total_on_disk += on_disk.len() as u64;
    }
    println!("{}", "-".repeat(60));
    let total_ratio = if total_on_disk == 0 {
        0.0
    } else {
        total_uncompressed as f64 / total_on_disk as f64
    };
    println!(
        "{:<18}  {:>14}  {:>14}  {:>7.2}x",
        "TOTAL", total_uncompressed, total_on_disk, total_ratio
    );

    Ok(())
}
