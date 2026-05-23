//! `parity-check` — runs the (eventual) algorithm against captured
//! production fixtures and reports passing-token / passing-revision
//! counts. The output of this binary is the ratchet the algorithm port
//! climbs.
//!
//! Current state: **stub**. The fixture loader and reporter work; the
//! algorithm-comparison step is unimplemented because the algorithm
//! itself isn't ported yet. Reported parity is 0% until that lands.
//!
//! Usage:
//!   parity-check                       # run against all fixtures
//!   parity-check en/534366             # run a single article
//!   parity-check --fixtures path/to/   # alternative fixtures root
//!
//! Future work (called out so the next session can pick it up):
//!   - Cache source wikitext per (lang, rev_id) under
//!     `parity-fixtures/.wikitext-cache/`; the algorithm needs the raw
//!     wikitext, which the captured fixtures don't include.
//!   - Implement actual comparison: parse `rev_content.json` for the
//!     expected token sequence, run the algorithm on the cached
//!     wikitext, diff token-by-token, count matches.
//!   - Output a machine-readable JSON summary alongside the human one
//!     so session notes can be auto-populated.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Subset of the wikiwho-api `rev_content` response we need to count
/// expected tokens. The full shape is documented in `API.md` §1; we
/// only deserialize the fields the parity check cares about.
#[derive(Debug, Deserialize)]
struct RevContent {
    article_title: String,
    page_id: u64,
    success: bool,
    // One object whose single key is the rev_id as a string. See API.md.
    revisions: Vec<BTreeMap<String, RevisionEntry>>,
}

#[derive(Debug, Deserialize)]
struct RevisionEntry {
    #[serde(default)]
    tokens: Vec<TokenEntry>,
}

#[derive(Debug, Deserialize)]
struct TokenEntry {
    // `str` is the only field guaranteed by API.md to be present; the
    // rest depend on query params. We capture only what we'll diff
    // against; deserializing-by-allow-extras keeps this forgiving.
    #[serde(rename = "str")]
    _str: String,
}

#[derive(Debug, Deserialize)]
struct Meta {
    lang: String,
    // title is captured for completeness but not currently used by the
    // stub. The future comparator will use it when emitting per-fixture
    // diff reports.
    #[allow(dead_code)]
    title: String,
    page_id: u64,
    rev_id: u64,
}

#[derive(Debug, Default)]
struct Tally {
    fixtures: u64,
    revisions_total: u64,
    revisions_passing: u64,
    tokens_total: u64,
    tokens_passing: u64,
    fixtures_failed_to_load: u64,
}

impl Tally {
    fn add_load_failure(&mut self) {
        self.fixtures += 1;
        self.fixtures_failed_to_load += 1;
    }

    fn add_fixture(&mut self, rev_count: u64, token_count: u64) {
        self.fixtures += 1;
        self.revisions_total += rev_count;
        self.tokens_total += token_count;
        // Once the algorithm exists, passing counts get set per-fixture
        // by the comparator. Until then, passing stays at 0.
    }

    fn report(&self, elapsed_ms: u128) {
        let pct = |num: u64, den: u64| {
            if den == 0 {
                "0.0".to_string()
            } else {
                format!("{:.2}", 100.0 * num as f64 / den as f64)
            }
        };
        println!();
        println!("parity-check summary");
        println!("--------------------");
        println!("  fixtures:           {}", self.fixtures);
        if self.fixtures_failed_to_load > 0 {
            println!("  failed to load:     {}", self.fixtures_failed_to_load);
        }
        println!(
            "  revisions passing:  {} / {} ({}%)",
            self.revisions_passing,
            self.revisions_total,
            pct(self.revisions_passing, self.revisions_total),
        );
        println!(
            "  tokens passing:     {} / {} ({}%)",
            self.tokens_passing,
            self.tokens_total,
            pct(self.tokens_passing, self.tokens_total),
        );
        println!("  elapsed:            {} ms", elapsed_ms);
        if self.revisions_passing == 0 && self.tokens_passing == 0 {
            println!();
            println!(
                "  note: comparison is STUBBED — the attribution algorithm \
                 hasn't been ported yet, so passing counts are pinned at 0. \
                 The numerator will start moving once \
                 wikiwho_attribute::analyse_article exists."
            );
        }
    }
}

fn default_fixtures_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at crates/wikiwho-parity/; walk up two
    // levels to reach the workspace root and join `parity-fixtures`.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    Path::new(manifest_dir)
        .join("..")
        .join("..")
        .join("parity-fixtures")
        .canonicalize()
        .unwrap_or_else(|_| Path::new(manifest_dir).join("../../parity-fixtures"))
}

fn parse_args() -> (PathBuf, Vec<String>) {
    let mut args = std::env::args().skip(1);
    let mut fixtures = default_fixtures_root();
    let mut filters = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--fixtures" => {
                fixtures = args
                    .next()
                    .map(PathBuf::from)
                    .expect("--fixtures requires a path");
            }
            "-h" | "--help" => {
                eprintln!("{}", env!("CARGO_PKG_DESCRIPTION"));
                eprintln!();
                eprintln!("Usage: parity-check [--fixtures DIR] [LANG/PAGE_ID ...]");
                std::process::exit(0);
            }
            other => filters.push(other.to_string()),
        }
    }
    (fixtures, filters)
}

/// Walk `<root>/<lang>/<page_id>/<rev_id>/` directories. Each leaf is
/// one fixture.
fn walk_fixtures(root: &Path, filters: &[String]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let langs = fs::read_dir(root)
        .with_context(|| format!("reading fixtures root {}", root.display()))?;
    for lang_entry in langs {
        let lang_dir = lang_entry?.path();
        if !lang_dir.is_dir() {
            continue;
        }
        let lang = lang_dir.file_name().and_then(|s| s.to_str()).unwrap_or("");
        for page_entry in fs::read_dir(&lang_dir)? {
            let page_dir = page_entry?.path();
            if !page_dir.is_dir() {
                continue;
            }
            let page_id = page_dir.file_name().and_then(|s| s.to_str()).unwrap_or("");
            let key = format!("{lang}/{page_id}");
            if !filters.is_empty()
                && !filters.iter().any(|f| {
                    key.starts_with(f) || lang == f || key == *f
                })
            {
                continue;
            }
            for rev_entry in fs::read_dir(&page_dir)? {
                let rev_dir = rev_entry?.path();
                if rev_dir.is_dir() {
                    out.push(rev_dir);
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

fn load_rev_content(path: &Path) -> Result<RevContent> {
    let bytes = fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {}", path.display()))
}

fn load_meta(path: &Path) -> Result<Meta> {
    let bytes = fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {}", path.display()))
}

fn process_one(fixture: &Path, tally: &mut Tally) -> Result<()> {
    let meta_path = fixture.join("meta.json");
    let rc_path = fixture.join("rev_content.json");
    if !meta_path.exists() || !rc_path.exists() {
        bail!(
            "{} missing meta.json and/or rev_content.json",
            fixture.display()
        );
    }
    let meta = load_meta(&meta_path)?;
    let rc = load_rev_content(&rc_path)?;

    if !rc.success {
        bail!(
            "{}/{} rev_content.success=false; refusing to parity-check",
            meta.lang,
            meta.rev_id
        );
    }
    if rc.page_id != meta.page_id {
        bail!(
            "{}: meta page_id={} disagrees with rev_content page_id={}",
            fixture.display(),
            meta.page_id,
            rc.page_id
        );
    }

    let rev_count: u64 = rc.revisions.len() as u64;
    let token_count: u64 = rc
        .revisions
        .iter()
        .flat_map(|m| m.values())
        .map(|r| r.tokens.len() as u64)
        .sum();

    println!(
        "  {}/{} {} (rev_id={}) — {} rev / {} tokens",
        meta.lang, meta.page_id, rc.article_title, meta.rev_id, rev_count, token_count
    );

    tally.add_fixture(rev_count, token_count);
    Ok(())
}

fn main() -> Result<()> {
    let (fixtures_root, filters) = parse_args();
    println!("fixtures root: {}", fixtures_root.display());
    if !filters.is_empty() {
        println!("filters:       {}", filters.join(", "));
    }
    let fixtures = walk_fixtures(&fixtures_root, &filters)
        .context("walking fixtures directory")?;
    if fixtures.is_empty() {
        bail!(
            "no fixtures found under {} matching {:?}",
            fixtures_root.display(),
            filters
        );
    }
    println!("loading {} fixture(s):", fixtures.len());

    let started = Instant::now();
    let mut tally = Tally::default();
    for fx in &fixtures {
        if let Err(e) = process_one(fx, &mut tally) {
            eprintln!("  SKIP {}: {:#}", fx.display(), e);
            tally.add_load_failure();
        }
    }
    tally.report(started.elapsed().as_millis());
    Ok(())
}
