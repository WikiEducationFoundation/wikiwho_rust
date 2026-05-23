//! `capture-history` — Rust replacement for `scripts/capture_history.py`.
//!
//! Walks `parity-fixtures/{lang}/{page_id}/{rev_id}/meta.json` and writes
//! `history.jsonl` next to each one. JSON shape is identical to the
//! Python script's so existing fixtures continue to parse.
//!
//! Usage:
//!   capture-history                       # all fixtures
//!   capture-history --only en/24544       # filter; substrings match
//!   capture-history --max-revs 30000      # abort fixtures over the cap
//!   capture-history --refresh             # re-fetch even if complete
//!   capture-history --between 0.3         # seconds between batches
//!
//! Idempotent: if `history.jsonl` already ends at the target rev_id,
//! skip the fixture. A partial file is removed on `--max-revs` abort
//! (a partial history is worse than no history).

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use wikiwho_mwclient::MwClient;

#[derive(Debug, Deserialize)]
struct Meta {
    lang: String,
    title: String,
    page_id: u64,
    rev_id: u64,
}

struct Args {
    only: Vec<String>,
    max_revs: Option<u64>,
    between: Duration,
    refresh: bool,
    fixtures_root: PathBuf,
}

fn parse_args() -> Result<Args> {
    let mut only = vec![];
    let mut max_revs = None;
    let mut between = Duration::from_millis(300);
    let mut refresh = false;
    let cwd = std::env::current_dir()?;
    let mut fixtures_root = cwd.join("parity-fixtures");

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--only" => only.push(args.next().context("--only needs a value")?),
            "--max-revs" => {
                max_revs = Some(
                    args.next()
                        .context("--max-revs needs a value")?
                        .parse::<u64>()?,
                )
            }
            "--between" => {
                let secs: f64 = args
                    .next()
                    .context("--between needs a value")?
                    .parse()?;
                between = Duration::from_secs_f64(secs);
            }
            "--refresh" => refresh = true,
            "--fixtures" => {
                fixtures_root = PathBuf::from(
                    args.next().context("--fixtures needs a value")?,
                );
            }
            "--help" | "-h" => {
                println!("{}", USAGE);
                std::process::exit(0);
            }
            other => bail!("unknown arg: {other}"),
        }
    }
    Ok(Args {
        only,
        max_revs,
        between,
        refresh,
        fixtures_root,
    })
}

const USAGE: &str = "\
capture-history — fetch revision histories for parity fixtures

  --only LANG/PAGE_ID    only run for matching fixture(s); accepts substrings
  --max-revs N           abort a fixture if its history exceeds N
  --between SECS         polite delay between API batches (default 0.3)
  --refresh              re-capture even if history.jsonl is complete
  --fixtures PATH        fixtures root (default: ./parity-fixtures)
";

fn main() -> Result<()> {
    let args = parse_args()?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(run(args))
}

async fn run(args: Args) -> Result<()> {
    let metas = find_metas(&args.fixtures_root)?;
    let metas = if args.only.is_empty() {
        metas
    } else {
        metas
            .into_iter()
            .filter(|(path, meta)| {
                let key = format!("{}/{}", meta.lang, meta.page_id);
                args.only.iter().any(|f| key.contains(f) || path.to_string_lossy().contains(f))
            })
            .collect()
    };
    if metas.is_empty() {
        bail!(
            "no fixtures matched (root={}, filters={:?})",
            args.fixtures_root.display(),
            args.only
        );
    }

    println!("capturing history for {} fixture(s)", metas.len());
    println!("polite delay: {:?} between batches", args.between);
    if let Some(c) = args.max_revs {
        println!("abort threshold: {c} revisions");
    }
    println!();

    // One client per language, reused across fixtures.
    let mut clients: BTreeMap<String, MwClient> = BTreeMap::new();
    let mut totals = Totals::default();

    for (meta_path, meta) in &metas {
        let client = match clients.get(&meta.lang) {
            Some(c) => c,
            None => {
                let c = MwClient::builder(&meta.lang)
                    .between_batches(args.between)
                    .build()
                    .with_context(|| format!("building client for lang={}", meta.lang))?;
                clients.entry(meta.lang.clone()).or_insert(c)
            }
        };

        let out_path = meta_path.parent().unwrap().join("history.jsonl");
        let rel = out_path
            .strip_prefix(args.fixtures_root.parent().unwrap_or(Path::new(".")))
            .unwrap_or(&out_path);

        if !args.refresh && already_complete(&out_path, meta.rev_id)? {
            println!("   skip (history.jsonl already ends at rev_id={}): {}",
                meta.rev_id,
                rel.display(),
            );
            totals.skipped += 1;
            println!();
            continue;
        }

        println!(
            "-> {}:{} (page_id={}, up to rev_id={})",
            meta.lang, meta.title, meta.page_id, meta.rev_id
        );

        match capture_one(client, meta, &out_path, args.max_revs).await {
            Ok(Outcome::Captured(n)) => {
                println!("   wrote {n} revisions to {}", rel.display());
                totals.captured += 1;
            }
            Ok(Outcome::Aborted(n)) => {
                println!(
                    "   ABORT: hit --max-revs cap after {n} revisions before reaching target. \
                     Removed partial file."
                );
                let _ = fs::remove_file(&out_path);
                totals.aborted += 1;
            }
            Err(e) => {
                println!("   FAILED: {e:?}");
                totals.failed += 1;
            }
        }
        println!();
    }

    println!(
        "done. captured={} skipped={} aborted={} failed={}",
        totals.captured, totals.skipped, totals.aborted, totals.failed
    );
    if totals.failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

#[derive(Default)]
struct Totals {
    captured: u64,
    skipped: u64,
    aborted: u64,
    failed: u64,
}

enum Outcome {
    Captured(u64),
    Aborted(u64),
}

async fn capture_one(
    client: &MwClient,
    meta: &Meta,
    out_path: &Path,
    max_revs: Option<u64>,
) -> Result<Outcome> {
    let mut fh = BufWriter::new(File::create(out_path)?);
    let mut fetcher = client.fetch_revisions(meta.page_id, meta.rev_id);
    let mut written: u64 = 0;
    let mut batches: u32 = 0;

    while let Some(batch) = fetcher.next_batch().await? {
        batches += 1;
        let n_this_batch = batch.revisions.len();
        let last_id = batch.revisions.last().map(|r| r.rev_id).unwrap_or(0);
        for rev in &batch.revisions {
            serde_json::to_writer(&mut fh, rev)?;
            fh.write_all(b"\n")?;
            written += 1;
        }
        if batches == 1 || batches % 10 == 0 || batch.saw_end {
            println!(
                "   batch {batches}: wrote {n_this_batch} revs (total {written}, last rev_id={last_id})"
            );
        }
        if let Some(cap) = max_revs {
            if written >= cap && !batch.saw_end {
                fh.flush()?;
                drop(fh);
                return Ok(Outcome::Aborted(written));
            }
        }
    }
    fh.flush()?;
    Ok(Outcome::Captured(written))
}

fn find_metas(root: &Path) -> Result<Vec<(PathBuf, Meta)>> {
    let mut out = vec![];
    walk(root, &mut |path| {
        if path.file_name().and_then(|n| n.to_str()) == Some("meta.json") {
            let txt = fs::read_to_string(path)?;
            let meta: Meta = serde_json::from_str(&txt)
                .with_context(|| format!("parsing {}", path.display()))?;
            out.push((path.to_path_buf(), meta));
        }
        Ok(())
    })?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn walk(dir: &Path, cb: &mut dyn FnMut(&Path) -> Result<()>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk(&path, cb)?;
        } else {
            cb(&path)?;
        }
    }
    Ok(())
}

fn already_complete(path: &Path, target_rev_id: u64) -> Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    if !path.exists() {
        return Ok(false);
    }
    let mut f = File::open(path)?;
    let size = f.seek(SeekFrom::End(0))?;
    if size == 0 {
        return Ok(false);
    }
    // A single Wikipedia revision's JSON can be hundreds of KB (the
    // article body lives inside it), so a fixed 4-KB tail read is
    // unreliable on real fixtures — it returns a JSON fragment, fails
    // to parse, and we trigger an unnecessary re-fetch. Read backwards
    // in chunks until we have either (a) a full final line bracketed
    // by a preceding newline, or (b) the whole file.
    let mut chunk: u64 = 16 * 1024;
    loop {
        let take = chunk.min(size);
        f.seek(SeekFrom::End(-(take as i64)))?;
        let mut buf = vec![0u8; take as usize];
        f.read_exact(&mut buf)?;
        // The file ends with a newline; trim trailing newlines first
        // so the rsplit logic finds the actual last record.
        let trimmed = trim_trailing_newlines(&buf);
        if let Some(newline_pos) = trimmed.iter().rposition(|&b| b == b'\n') {
            let last_line = &trimmed[newline_pos + 1..];
            return Ok(line_matches_target(last_line, target_rev_id));
        }
        // No newline in the buffer yet — either need a bigger window
        // or the whole file is one line.
        if take == size {
            return Ok(line_matches_target(trimmed, target_rev_id));
        }
        chunk = chunk.saturating_mul(4);
    }
}

fn trim_trailing_newlines(buf: &[u8]) -> &[u8] {
    let mut end = buf.len();
    while end > 0 && buf[end - 1] == b'\n' {
        end -= 1;
    }
    &buf[..end]
}

fn line_matches_target(line: &[u8], target_rev_id: u64) -> bool {
    let Ok(s) = std::str::from_utf8(line) else {
        return false;
    };
    let Ok(obj) = serde_json::from_str::<serde_json::Value>(s) else {
        return false;
    };
    obj.get("rev_id")
        .and_then(|v| v.as_u64())
        .map(|r| r == target_rev_id)
        .unwrap_or(false)
}
