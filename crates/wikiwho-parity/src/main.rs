//! `parity-check` — compares Rust algorithm output to captured
//! production fixtures and reports passing-token / passing-revision
//! counts. The output of this binary is the ratchet the algorithm port
//! climbs.
//!
//! Current comparison level: **tokenizer parity** — for each fixture
//! we read the cached wikitext, lowercase it, walk it through
//! `wikiwho_attribute::tokenize::tokenize_revision`, and compare the
//! resulting token strings positionally to `rev_content.json`'s
//! `tokens[i].str`. This validates the tokenizer + paragraph/sentence
//! splitter against real Wikipedia text; it does NOT yet validate the
//! attribution algorithm (which needs the matching cascade + Myers
//! diff + multi-revision input).
//!
//! Usage:
//!   parity-check                       # run against all fixtures
//!   parity-check en/534366             # run a single article
//!   parity-check --fixtures path/to/   # alternative fixtures root
//!   parity-check --show-first-diff     # print first divergence per fixture
//!
//! Future work:
//!   - True algorithm parity: requires multi-revision input (full
//!     history up to the target rev_id) and the ported algorithm.
//!     For now we exercise only the input side of the pipeline.
//!   - Machine-readable JSON summary so session notes can be
//!     auto-populated.

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
    text: String,
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

    fn merge(&mut self, c: &ComparisonResult) {
        self.fixtures += 1;
        self.revisions_total += c.rev_count;
        self.revisions_passing += c.rev_passing;
        self.tokens_total += c.token_count;
        self.tokens_passing += c.token_passing;
    }

    fn report(&self, elapsed_ms: u128) {
        let pct = |num: u64, den: u64| {
            if den == 0 {
                "0.00".to_string()
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
        println!();
        println!(
            "  note: tokenizer-level parity only. The attribution \
             algorithm (matching cascade, Myers diff, multi-rev history) \
             isn't ported yet, so o_rev_id / token_id / in / out aren't \
             validated. This number reflects whether the paragraph / \
             sentence / token splitter agrees with the reference on real \
             Wikipedia text."
        );
    }
}

#[derive(Debug, Default)]
struct ComparisonResult {
    rev_count: u64,
    rev_passing: u64,
    token_count: u64,
    token_passing: u64,
    /// First divergence as `(position, rust, expected)` — populated
    /// for diagnostics, only printed when `--show-first-diff` is set.
    first_diff: Option<(usize, String, String)>,
    /// Lengths as `(rust, expected)` when they differ.
    length_mismatch: Option<(usize, usize)>,
}

fn compare(rust: &[String], expected: &[TokenEntry]) -> ComparisonResult {
    let token_count = expected.len() as u64;
    let mut token_passing = 0u64;
    let mut first_diff = None;

    for (i, exp) in expected.iter().enumerate() {
        match rust.get(i) {
            Some(got) if got == &exp.text => token_passing += 1,
            Some(got) => {
                if first_diff.is_none() {
                    first_diff = Some((i, got.clone(), exp.text.clone()));
                }
            }
            None => {
                if first_diff.is_none() {
                    first_diff = Some((i, String::new(), exp.text.clone()));
                }
            }
        }
    }

    let length_mismatch = if rust.len() == expected.len() {
        None
    } else {
        Some((rust.len(), expected.len()))
    };

    // A revision counts as passing only when EVERY position matches AND
    // lengths agree — anything looser would be misleading.
    let rev_passing = if length_mismatch.is_none() && token_passing == token_count {
        1
    } else {
        0
    };

    ComparisonResult {
        rev_count: 1,
        rev_passing,
        token_count,
        token_passing,
        first_diff,
        length_mismatch,
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

struct Args {
    fixtures: PathBuf,
    filters: Vec<String>,
    show_first_diff: bool,
}

fn parse_args() -> Args {
    let mut args = std::env::args().skip(1);
    let mut out = Args {
        fixtures: default_fixtures_root(),
        filters: Vec::new(),
        show_first_diff: false,
    };
    while let Some(a) = args.next() {
        match a.as_str() {
            "--fixtures" => {
                out.fixtures = args
                    .next()
                    .map(PathBuf::from)
                    .expect("--fixtures requires a path");
            }
            "--show-first-diff" => out.show_first_diff = true,
            "-h" | "--help" => {
                eprintln!("{}", env!("CARGO_PKG_DESCRIPTION"));
                eprintln!();
                eprintln!(
                    "Usage: parity-check [--fixtures DIR] [--show-first-diff] [LANG/PAGE_ID ...]"
                );
                std::process::exit(0);
            }
            other => out.filters.push(other.to_string()),
        }
    }
    out
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

fn process_one(fixture: &Path, args: &Args, tally: &mut Tally) -> Result<()> {
    let meta_path = fixture.join("meta.json");
    let rc_path = fixture.join("rev_content.json");
    let wt_path = fixture.join("wikitext.txt");
    if !meta_path.exists() || !rc_path.exists() {
        bail!(
            "{} missing meta.json and/or rev_content.json",
            fixture.display()
        );
    }
    if !wt_path.exists() {
        bail!(
            "{} missing wikitext.txt — run scripts/cache_wikitext.py first",
            fixture.display()
        );
    }
    let meta = load_meta(&meta_path)?;
    let rc = load_rev_content(&rc_path)?;
    let wikitext = fs::read_to_string(&wt_path)
        .with_context(|| format!("reading {}", wt_path.display()))?;

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

    // The reference algorithm lowercases at wikiwho.py:123 / :191 before
    // tokenization. Mirror that here so the Rust output is comparable
    // to the captured (already-lowercased) fixture tokens.
    let lowered = wikitext.to_lowercase();
    let rust_tokens = wikiwho_attribute::tokenize::tokenize_revision(&lowered);

    let mut total = ComparisonResult::default();
    for rev_map in &rc.revisions {
        // Each fixture's `revisions` field is a list-of-single-key-maps
        // (see API.md §1) keyed by rev_id-as-string. For now every
        // fixture contains exactly one entry; future multi-rev fixtures
        // need a richer compare that splits the Rust output per rev.
        for entry in rev_map.values() {
            let c = compare(&rust_tokens, &entry.tokens);
            total.rev_count += c.rev_count;
            total.rev_passing += c.rev_passing;
            total.token_count += c.token_count;
            total.token_passing += c.token_passing;
            if total.first_diff.is_none() {
                total.first_diff = c.first_diff;
            }
            if total.length_mismatch.is_none() {
                total.length_mismatch = c.length_mismatch;
            }
        }
    }

    let pass_pct = if total.token_count == 0 {
        0.0
    } else {
        100.0 * total.token_passing as f64 / total.token_count as f64
    };
    let pass_marker = if total.rev_passing == total.rev_count {
        "PASS"
    } else {
        "FAIL"
    };
    println!(
        "  [{}] {}/{} {} (rev_id={}) — {} / {} tokens ({:.2}%)",
        pass_marker,
        meta.lang,
        meta.page_id,
        rc.article_title,
        meta.rev_id,
        total.token_passing,
        total.token_count,
        pass_pct,
    );
    if let Some((rust_len, exp_len)) = total.length_mismatch {
        println!(
            "         length: rust={} expected={} (Δ={:+})",
            rust_len,
            exp_len,
            rust_len as i64 - exp_len as i64,
        );
    }
    if args.show_first_diff {
        if let Some((i, got, exp)) = &total.first_diff {
            println!(
                "         first diff @ {}: rust={:?} expected={:?}",
                i, got, exp
            );
        }
    }

    tally.merge(&total);
    Ok(())
}

fn main() -> Result<()> {
    let args = parse_args();
    println!("fixtures root: {}", args.fixtures.display());
    if !args.filters.is_empty() {
        println!("filters:       {}", args.filters.join(", "));
    }
    let fixtures = walk_fixtures(&args.fixtures, &args.filters)
        .context("walking fixtures directory")?;
    if fixtures.is_empty() {
        bail!(
            "no fixtures found under {} matching {:?}",
            args.fixtures.display(),
            args.filters,
        );
    }
    println!("loading {} fixture(s):", fixtures.len());

    let started = Instant::now();
    let mut tally = Tally::default();
    for fx in &fixtures {
        if let Err(e) = process_one(fx, &args, &mut tally) {
            eprintln!("  SKIP {}: {:#}", fx.display(), e);
            tally.add_load_failure();
        }
    }
    tally.report(started.elapsed().as_millis());
    Ok(())
}
