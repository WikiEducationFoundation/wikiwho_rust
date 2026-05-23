//! `parity-check` — compares Rust algorithm output to captured
//! production fixtures and reports passing-token / passing-revision
//! counts. The output of this binary is the ratchet the algorithm port
//! climbs.
//!
//! Current comparison level: **cascade single-rev parity** — for each
//! fixture we read the cached wikitext, run it through
//! `Article::analyse_revision` (full paragraph + sentence +
//! insertion-only token cascade), and compare the resulting
//! `article.tokens[i].value` to `rev_content.json`'s `tokens[i].str`.
//! This exercises the splitter AND the cascade plumbing end-to-end
//! on real Wikipedia text; the headline percentage matches the
//! tokenizer-only number because both walk the same splitter — the
//! cascade adds metadata (token_id, origin_rev_id, in, out) that
//! single-rev fixtures cannot validate.
//!
//! Single-rev fixtures don't exercise the cascade's general-case Differ
//! path (text_prev is empty for the first revision). The full-history
//! mode (`--full-history`) replays every revision in order and is the
//! real algorithm-parity ratchet.
//!
//! Usage:
//!   parity-check                       # run against all fixtures
//!   parity-check en/534366             # run a single article
//!   parity-check --fixtures path/to/   # alternative fixtures root
//!   parity-check --show-first-diff     # print first divergence per fixture
//!   parity-check --full-history        # opt-in multi-rev mode: feed every
//!                                      # rev from history.jsonl in order
//!                                      # and compare metadata (o_rev_id,
//!                                      # inbound, outbound), not just
//!                                      # token strings
//!
//! Full-history mode requires `history.jsonl` per fixture
//! (`scripts/capture_history.py`). Fixtures without history are skipped
//! with a note.
//!
//! Future work:
//!   - Machine-readable JSON summary so session notes can be
//!     auto-populated.
//!   - Production endpoint compare-to-spec: serve our Article via the
//!     wire format from API.md and diff full JSON responses.

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

#[derive(Debug, Clone, Deserialize)]
struct TokenEntry {
    // `str` is the only field guaranteed by API.md to be present; the
    // rest depend on query params. We capture only what we'll diff
    // against; deserializing-by-allow-extras keeps this forgiving.
    #[serde(rename = "str")]
    text: String,
    // The remaining fields are only present in fixtures captured with
    // the full parameter set (which our `capture_fixtures.py` does).
    // Missing → None, used only by `--full-history` mode.
    #[serde(default)]
    o_rev_id: Option<u64>,
    #[serde(default, rename = "in")]
    inbound: Vec<u64>,
    #[serde(default, rename = "out")]
    outbound: Vec<u64>,
}

#[derive(Debug, Deserialize)]
struct Meta {
    lang: String,
    title: String,
    page_id: u64,
    rev_id: u64,
}

/// One line of `history.jsonl` (see `scripts/capture_history.py`).
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

/// Output of `scripts/python_replay.py`. We only consume `final_tokens`;
/// the rest is for human inspection. The `final_tokens` shape is
/// compatible with `RevisionEntry.tokens` (rev_content.json's tokens
/// array), so the same comparator works for both ground-truth sources.
#[derive(Debug, Deserialize)]
struct PythonReplay {
    target_rev_id: u64,
    #[serde(default)]
    final_tokens: Option<Vec<TokenEntry>>,
}

#[derive(Debug, Default)]
struct Tally {
    fixtures: u64,
    revisions_total: u64,
    revisions_passing: u64,
    tokens_total: u64,
    tokens_passing: u64,
    fixtures_failed_to_load: u64,
    // Per-field counters: only incremented in full-history mode.
    // `tokens_str_passing` is the same metric as `tokens_passing` —
    // duplicated here so the per-field report reads cleanly.
    tokens_str_passing: u64,
    tokens_o_rev_id_passing: u64,
    tokens_inbound_passing: u64,
    tokens_outbound_passing: u64,
    tokens_all_passing: u64,
    full_history: bool,
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
        self.tokens_str_passing += c.token_passing;
        self.tokens_o_rev_id_passing += c.o_rev_id_passing;
        self.tokens_inbound_passing += c.inbound_passing;
        self.tokens_outbound_passing += c.outbound_passing;
        self.tokens_all_passing += c.all_fields_passing;
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
        if self.full_history {
            println!(
                "  ├─ str:             {} / {} ({}%)",
                self.tokens_str_passing,
                self.tokens_total,
                pct(self.tokens_str_passing, self.tokens_total),
            );
            println!(
                "  ├─ o_rev_id:        {} / {} ({}%)",
                self.tokens_o_rev_id_passing,
                self.tokens_total,
                pct(self.tokens_o_rev_id_passing, self.tokens_total),
            );
            println!(
                "  ├─ inbound:         {} / {} ({}%)",
                self.tokens_inbound_passing,
                self.tokens_total,
                pct(self.tokens_inbound_passing, self.tokens_total),
            );
            println!(
                "  ├─ outbound:        {} / {} ({}%)",
                self.tokens_outbound_passing,
                self.tokens_total,
                pct(self.tokens_outbound_passing, self.tokens_total),
            );
            println!(
                "  └─ all-fields:      {} / {} ({}%)",
                self.tokens_all_passing,
                self.tokens_total,
                pct(self.tokens_all_passing, self.tokens_total),
            );
        }
        println!("  elapsed:            {} ms", elapsed_ms);
        println!();
        if self.full_history {
            println!(
                "  note: full-history parity. Each fixture's history.jsonl \
                 is replayed in order through Article::analyse_revision; \
                 the final-revision token stream is then compared to the \
                 reference (production wikiwho-api by default, or a fresh \
                 Python run with --python-replay). The all-fields \
                 percentage is the real algorithm-parity number — \
                 anything below 100% (vs Python ground truth) is a bug \
                 in the port, since the Rust cascade now uses a faithful \
                 port of Python's Differ."
            );
        } else {
            println!(
                "  note: cascade single-rev parity. The full paragraph + \
                 sentence + insertion-only token cascade runs end-to-end on \
                 each fixture, but `o_rev_id` / `token_id` / `in` / `out` \
                 aren't compared yet — single-rev fixtures can't validate \
                 them. Pass --full-history (and run \
                 scripts/capture_history.py first) to enable the multi-rev \
                 ratchet."
            );
        }
    }
}

#[derive(Debug, Default)]
struct ComparisonResult {
    rev_count: u64,
    rev_passing: u64,
    token_count: u64,
    token_passing: u64,
    o_rev_id_passing: u64,
    inbound_passing: u64,
    outbound_passing: u64,
    /// Tokens where str AND o_rev_id AND in AND out all match.
    all_fields_passing: u64,
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
        ..Default::default()
    }
}

/// Full-history comparison: walks every token position and counts
/// per-field passing as well as a strict "all fields match" predicate.
///
/// Rust input is a slice of `&Word` pulled from `iter_rev_tokens` on
/// the target revision; expected is the production wikiwho-api token
/// list. The two are expected to be the same length; if not, the
/// shorter side bounds the iteration and the missing positions count
/// as failing for every field.
fn compare_full(
    rust: &[&wikiwho_attribute::structures::Word],
    expected: &[TokenEntry],
) -> ComparisonResult {
    let token_count = expected.len() as u64;
    let mut token_passing = 0u64;
    let mut o_rev_id_passing = 0u64;
    let mut inbound_passing = 0u64;
    let mut outbound_passing = 0u64;
    let mut all_fields_passing = 0u64;
    let mut first_diff = None;

    for (i, exp) in expected.iter().enumerate() {
        let Some(got) = rust.get(i) else {
            if first_diff.is_none() {
                first_diff = Some((i, String::new(), exp.text.clone()));
            }
            continue;
        };
        let str_ok = got.value == exp.text;
        if str_ok {
            token_passing += 1;
        } else if first_diff.is_none() {
            first_diff = Some((i, got.value.clone(), exp.text.clone()));
        }

        let o_ok = exp.o_rev_id.map(|exp_id| exp_id == got.origin_rev_id).unwrap_or(true);
        if o_ok {
            o_rev_id_passing += 1;
        }
        let in_ok = exp.inbound == got.inbound;
        if in_ok {
            inbound_passing += 1;
        }
        let out_ok = exp.outbound == got.outbound;
        if out_ok {
            outbound_passing += 1;
        }
        if str_ok && o_ok && in_ok && out_ok {
            all_fields_passing += 1;
        }
    }

    let length_mismatch = if rust.len() == expected.len() {
        None
    } else {
        Some((rust.len(), expected.len()))
    };

    let rev_passing = if length_mismatch.is_none() && all_fields_passing == token_count {
        1
    } else {
        0
    };

    ComparisonResult {
        rev_count: 1,
        rev_passing,
        token_count,
        token_passing,
        o_rev_id_passing,
        inbound_passing,
        outbound_passing,
        all_fields_passing,
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
    full_history: bool,
    /// Print up to N tokens whose inbound/outbound list disagrees with
    /// production, showing the symmetric difference of rev_ids. Useful
    /// for tracing inflation patterns. Only meaningful with
    /// `--full-history`.
    show_field_mismatches: usize,
    /// Print every rev_id our cascade flagged as spam, in the order it
    /// was caught. Useful for cross-checking against expected
    /// vandalism. Only meaningful with `--full-history`.
    show_spam_ids: bool,
    /// Print the top-N rev_ids by absolute mismatch in inbound/outbound
    /// mention count between rust and production. Tells you which
    /// specific revisions are over- or under-recorded systematically
    /// — much higher signal than per-token mismatches once the floor
    /// stops being "everything is wrong." Only meaningful with
    /// `--full-history`.
    rev_id_histogram: usize,
    /// Cap full-history replay at this many revisions. Useful for
    /// binary-searching when divergences first appear, and for fast
    /// iteration on small slices.
    max_revs: Option<usize>,
    /// In full-history mode, compare against a fresh run of the
    /// reference Python wikiwho.py instead of the captured
    /// rev_content.json. Sage's directive: the production cache may
    /// have evolved over years, but a fresh-from-scratch Python run
    /// on the same history.jsonl is the real reference. The Python
    /// output is cached at <fixture>/python_replay.json so subsequent
    /// runs are fast.
    python_replay: bool,
    /// Force a re-run of the Python reference even if a cached
    /// python_replay.json exists. Used after fixture or capture-script
    /// changes.
    refresh_python: bool,
}

fn parse_args() -> Args {
    let mut args = std::env::args().skip(1);
    let mut out = Args {
        fixtures: default_fixtures_root(),
        filters: Vec::new(),
        show_first_diff: false,
        full_history: false,
        show_field_mismatches: 0,
        show_spam_ids: false,
        rev_id_histogram: 0,
        max_revs: None,
        python_replay: false,
        refresh_python: false,
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
            "--full-history" => out.full_history = true,
            "--show-field-mismatches" => {
                out.show_field_mismatches = args
                    .next()
                    .and_then(|s| s.parse().ok())
                    .expect("--show-field-mismatches requires a positive integer");
            }
            "--show-spam-ids" => out.show_spam_ids = true,
            "--rev-id-histogram" => {
                out.rev_id_histogram = args
                    .next()
                    .and_then(|s| s.parse().ok())
                    .expect("--rev-id-histogram requires a positive integer");
            }
            "--max-revs" => {
                out.max_revs = Some(
                    args.next()
                        .and_then(|s| s.parse().ok())
                        .expect("--max-revs requires a positive integer"),
                );
            }
            "--python-replay" => out.python_replay = true,
            "--refresh-python" => {
                out.python_replay = true;
                out.refresh_python = true;
            }
            "-h" | "--help" => {
                eprintln!("{}", env!("CARGO_PKG_DESCRIPTION"));
                eprintln!();
                eprintln!(
                    "Usage: parity-check [--fixtures DIR] [--show-first-diff] \
                     [--full-history] [--show-field-mismatches N] \
                     [--show-spam-ids] [--rev-id-histogram N] [--max-revs N] \
                     [--python-replay] [--refresh-python] \
                     [LANG/PAGE_ID ...]"
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

/// Load a fixture's Python-reference token sequence, regenerating it
/// via `scripts/python_replay.py` if absent or `--refresh-python` was
/// set. The cache lives at `<fixture>/python_replay.json` and is
/// regeneratable from `history.jsonl` alone (no MW or production API
/// dependency).
fn load_python_replay(fixture: &Path, refresh: bool) -> Result<PythonReplay> {
    let cache = fixture.join("python_replay.json");
    if !cache.exists() || refresh {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("scripts")
            .join("python_replay.py");
        eprintln!(
            "  [python] regenerating {} via {}",
            cache.display(),
            script.display()
        );
        let output = std::process::Command::new("python3")
            .arg(&script)
            .arg(fixture)
            .output()
            .with_context(|| format!("invoking python3 {}", script.display()))?;
        if !output.status.success() {
            bail!(
                "python_replay.py failed (status {}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr),
            );
        }
        fs::write(&cache, &output.stdout)
            .with_context(|| format!("writing {}", cache.display()))?;
    }
    let bytes = fs::read(&cache)
        .with_context(|| format!("reading {}", cache.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {}", cache.display()))
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

    // Run the wikitext through the full cascade. `analyse_revision`
    // lowercases internally (wikiwho.py:123 / :191) before any
    // tokenizer call, so we pass the raw wikitext. For tokenizer-only
    // parity (the current ratchet) the cascade and the splitter
    // produce identical output; the cascade also populates
    // `article.tokens` with Word metadata that future parity levels
    // will validate.
    let mut article = wikiwho_attribute::structures::Article::new(&meta.title);
    let outcome = article.analyse_revision(wikiwho_attribute::pipeline::RevisionInput {
        rev_id: meta.rev_id,
        timestamp: String::from("1970-01-01T00:00:00Z"),
        text: wikitext,
        sha1: None,
        comment: None,
        minor: false,
        user_id: None,
        user_name: None,
    });
    if let wikiwho_attribute::pipeline::RevisionOutcome::Vandalism(reason) = outcome {
        bail!(
            "{}/{} rev_id={} flagged as vandalism ({:?}); the parity \
             corpus is curated production revisions, this is a cascade \
             bug",
            meta.lang,
            meta.page_id,
            meta.rev_id,
            reason
        );
    }
    let rust_tokens: Vec<String> = article.tokens.iter().map(|w| w.value.clone()).collect();

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

fn process_one_full_history(fixture: &Path, args: &Args, tally: &mut Tally) -> Result<()> {
    let meta_path = fixture.join("meta.json");
    let rc_path = fixture.join("rev_content.json");
    let history_path = fixture.join("history.jsonl");
    if !meta_path.exists() || !rc_path.exists() {
        bail!("{} missing meta.json and/or rev_content.json", fixture.display());
    }
    if !history_path.exists() {
        bail!(
            "{} missing history.jsonl — run scripts/capture_history.py first",
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

    let history_text = fs::read_to_string(&history_path)
        .with_context(|| format!("reading {}", history_path.display()))?;
    let mut entries: Vec<HistoryEntry> = history_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str::<HistoryEntry>(l)
                .with_context(|| format!("parsing line of {}", history_path.display()))
        })
        .collect::<Result<_>>()?;
    if let Some(cap) = args.max_revs {
        entries.truncate(cap);
    }

    let mut article = wikiwho_attribute::structures::Article::new(&meta.title);
    article.page_id = Some(meta.page_id);
    let mut fed = 0u64;
    let mut skipped_hidden = 0u64;
    let mut spam_count = 0u64;

    for entry in &entries {
        if entry.text_hidden {
            // Mirror wikiwho.py:146 — `texthidden` / `textmissing`
            // revisions never enter the algorithm.
            skipped_hidden += 1;
            continue;
        }
        let outcome = article.analyse_revision(wikiwho_attribute::pipeline::RevisionInput {
            rev_id: entry.rev_id,
            timestamp: entry.timestamp.clone(),
            text: entry.text.clone(),
            sha1: entry.sha1.clone(),
            comment: entry.comment.clone(),
            minor: entry.minor,
            user_id: entry.user_id,
            user_name: entry.user_name.clone(),
        });
        fed += 1;
        if matches!(
            outcome,
            wikiwho_attribute::pipeline::RevisionOutcome::Vandalism(_)
        ) {
            spam_count += 1;
        }
    }

    // Pull the final revision and compare its token stream. When
    // `--max-revs` truncated the input, the target rev_id may not be
    // present — in that case we still print the structural state so
    // the user can see how it evolved over the slice, but skip the
    // production-comparison block.
    let final_rev = match article.revisions.get(&meta.rev_id) {
        Some(r) => r,
        None if args.max_revs.is_some() => {
            println!(
                "  [SKIP-CMP] {}/{} {} — target rev_id={} not in replay \
                 (--max-revs cap), reporting state only",
                meta.lang, meta.page_id, rc.article_title, meta.rev_id,
            );
            println!(
                "         replayed {} of {} (capped) revs, hidden {}, spam {}",
                fed, entries.len(), skipped_hidden, spam_count,
            );
            if args.show_spam_ids {
                let mut ids = article.spam_ids.clone();
                ids.sort();
                println!("         spam_ids ({}): {:?}", ids.len(), ids);
                println!(
                    "         arena: tokens={} sentences={} paragraphs={} | ht: \
                     paragraphs_ht={} sentences_ht={} | processed_revs={}",
                    article.tokens.len(),
                    article.sentences.len(),
                    article.paragraphs.len(),
                    article.paragraphs_ht.len(),
                    article.sentences_ht.len(),
                    article.revisions.len(),
                );
            }
            return Ok(());
        }
        None => bail!(
            "{}/{} target rev_id={} not in article.revisions after replay \
             (fed {} of {} input revs, hidden {}, spam {}). Either the \
             history doesn't actually reach the target, or our algorithm \
             flagged the target itself as spam.",
            meta.lang,
            meta.page_id,
            meta.rev_id,
            fed,
            entries.len(),
            skipped_hidden,
            spam_count,
        ),
    };
    let final_token_ids = wikiwho_attribute::structures::iter_rev_tokens(&article, final_rev);
    let rust_words: Vec<&wikiwho_attribute::structures::Word> = final_token_ids
        .iter()
        .map(|id| article.word(*id))
        .collect();

    // Resolve the expected-tokens source. With --python-replay, we use a
    // fresh Python run (cached at <fixture>/python_replay.json); without
    // it, we use the captured production rev_content.json. Per Sage:
    // production caches may have evolved over years, so the Python run
    // is the real reference for algorithm parity.
    let expected_tokens: Vec<TokenEntry> = if args.python_replay {
        let pr = load_python_replay(fixture, args.refresh_python)?;
        if pr.target_rev_id != meta.rev_id {
            bail!(
                "{}: python_replay.json target_rev_id={} disagrees with meta rev_id={}",
                fixture.display(),
                pr.target_rev_id,
                meta.rev_id,
            );
        }
        pr.final_tokens.ok_or_else(|| anyhow::anyhow!(
            "{}: python_replay.json has final_tokens=null — Python flagged \
             the target revision as spam? Re-run with --refresh-python after \
             fixing the input.",
            fixture.display(),
        ))?
    } else {
        rc.revisions
            .iter()
            .flat_map(|m| m.values().flat_map(|e| e.tokens.iter().cloned()))
            .collect()
    };

    let mut total = ComparisonResult::default();
    let c = compare_full(&rust_words, &expected_tokens);
    total.rev_count += c.rev_count;
    total.rev_passing += c.rev_passing;
    total.token_count += c.token_count;
    total.token_passing += c.token_passing;
    total.o_rev_id_passing += c.o_rev_id_passing;
    total.inbound_passing += c.inbound_passing;
    total.outbound_passing += c.outbound_passing;
    total.all_fields_passing += c.all_fields_passing;
    total.first_diff = c.first_diff;
    total.length_mismatch = c.length_mismatch;

    let pass_pct_all = if total.token_count == 0 {
        0.0
    } else {
        100.0 * total.all_fields_passing as f64 / total.token_count as f64
    };
    let pass_pct_str = if total.token_count == 0 {
        0.0
    } else {
        100.0 * total.token_passing as f64 / total.token_count as f64
    };
    let pass_marker = if total.rev_passing == total.rev_count {
        "PASS"
    } else {
        "FAIL"
    };
    let source_tag = if args.python_replay { "vs python" } else { "vs prod-cache" };
    println!(
        "  [{}] {}/{} {} (rev_id={}, {source_tag}) — replayed {} of {} revs ({} hidden, \
         {} spam) — str {} / {} ({:.2}%), all-fields {} / {} ({:.2}%)",
        pass_marker,
        meta.lang,
        meta.page_id,
        rc.article_title,
        meta.rev_id,
        fed,
        entries.len(),
        skipped_hidden,
        spam_count,
        total.token_passing,
        total.token_count,
        pass_pct_str,
        total.all_fields_passing,
        total.token_count,
        pass_pct_all,
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
            println!("         first str diff @ {}: rust={:?} expected={:?}", i, got, exp);
        }
    }
    if args.show_spam_ids {
        let mut ids = article.spam_ids.clone();
        ids.sort();
        println!("         spam_ids ({}): {:?}", ids.len(), ids);
        let p_ht_total: usize = article.paragraphs_ht.values().map(Vec::len).sum();
        let s_ht_total: usize = article.sentences_ht.values().map(Vec::len).sum();
        println!(
            "         arena: tokens={} sentences={} paragraphs={} | ht hashes: \
             paragraphs_ht={} sentences_ht={} | ht totals: p={} s={} | processed_revs={}",
            article.tokens.len(),
            article.sentences.len(),
            article.paragraphs.len(),
            article.paragraphs_ht.len(),
            article.sentences_ht.len(),
            p_ht_total,
            s_ht_total,
            article.revisions.len(),
        );
    }
    if args.rev_id_histogram > 0 {
        // For each rev_id mentioned anywhere in inbound or outbound,
        // count rust vs production occurrences across all tokens. Sort
        // by |rust - exp| descending and print the top-N. This isolates
        // *which revisions* drive the divergence, regardless of which
        // tokens they affect.
        use std::collections::BTreeMap;
        let mut rust_in: BTreeMap<u64, u64> = BTreeMap::new();
        let mut rust_out: BTreeMap<u64, u64> = BTreeMap::new();
        let mut exp_in: BTreeMap<u64, u64> = BTreeMap::new();
        let mut exp_out: BTreeMap<u64, u64> = BTreeMap::new();
        for word in &rust_words {
            for &r in &word.inbound {
                *rust_in.entry(r).or_default() += 1;
            }
            for &r in &word.outbound {
                *rust_out.entry(r).or_default() += 1;
            }
        }
        for tok in &expected_tokens {
            for &r in &tok.inbound {
                *exp_in.entry(r).or_default() += 1;
            }
            for &r in &tok.outbound {
                *exp_out.entry(r).or_default() += 1;
            }
        }
        let mut all_revs: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
        all_revs.extend(rust_in.keys());
        all_revs.extend(rust_out.keys());
        all_revs.extend(exp_in.keys());
        all_revs.extend(exp_out.keys());
        let mut rows: Vec<(u64, u64, u64, u64, u64, i64)> = all_revs
            .into_iter()
            .map(|r| {
                let ri = *rust_in.get(&r).unwrap_or(&0);
                let ro = *rust_out.get(&r).unwrap_or(&0);
                let ei = *exp_in.get(&r).unwrap_or(&0);
                let eo = *exp_out.get(&r).unwrap_or(&0);
                let abs_diff = (ri as i64 - ei as i64).abs() + (ro as i64 - eo as i64).abs();
                (r, ri, ro, ei, eo, abs_diff)
            })
            .filter(|(_, _, _, _, _, d)| *d > 0)
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.5));
        let total_divergent = rows.len();
        println!(
            "         rev_id-histogram: {} divergent rev_ids (showing top {})",
            total_divergent,
            args.rev_id_histogram.min(total_divergent),
        );
        println!(
            "         {:>10}  {:>7} {:>7} {:>7} {:>7}  {:>7}",
            "rev_id", "r_in", "r_out", "e_in", "e_out", "|diff|",
        );
        for (rev, ri, ro, ei, eo, d) in rows.iter().take(args.rev_id_histogram) {
            println!(
                "         {:>10}  {:>7} {:>7} {:>7} {:>7}  {:>+7}",
                rev, ri, ro, ei, eo, d,
            );
        }
    }
    if args.show_field_mismatches > 0 {
        // Re-walk and report up to N tokens where in/out diverged. Sets
        // (rather than vector equality) are reported because order is
        // not part of the contract — but vector order should also match
        // expected in practice. The set diff is what tells us which
        // rev_ids one side has that the other doesn't.
        use std::collections::HashSet;
        let mut shown = 0usize;
        for (i, exp) in expected_tokens.iter().enumerate() {
            if shown >= args.show_field_mismatches {
                break;
            }
            let Some(got) = rust_words.get(i) else { continue };
            let in_ok = exp.inbound == got.inbound;
            let out_ok = exp.outbound == got.outbound;
            if in_ok && out_ok {
                continue;
            }
            let rust_in: HashSet<u64> = got.inbound.iter().copied().collect();
            let exp_in: HashSet<u64> = exp.inbound.iter().copied().collect();
            let rust_out: HashSet<u64> = got.outbound.iter().copied().collect();
            let exp_out: HashSet<u64> = exp.outbound.iter().copied().collect();
            println!(
                "         token #{i} {:?} (id={}, origin={}, last={})",
                got.value, got.token_id, got.origin_rev_id, got.last_rev_id,
            );
            if !in_ok {
                let only_rust: Vec<u64> = rust_in.difference(&exp_in).copied().collect();
                let only_exp: Vec<u64> = exp_in.difference(&rust_in).copied().collect();
                println!(
                    "           inbound:  rust={} expected={}  rust-only={:?} expected-only={:?}",
                    got.inbound.len(), exp.inbound.len(),
                    sorted(only_rust), sorted(only_exp),
                );
            }
            if !out_ok {
                let only_rust: Vec<u64> = rust_out.difference(&exp_out).copied().collect();
                let only_exp: Vec<u64> = exp_out.difference(&rust_out).copied().collect();
                println!(
                    "           outbound: rust={} expected={}  rust-only={:?} expected-only={:?}",
                    got.outbound.len(), exp.outbound.len(),
                    sorted(only_rust), sorted(only_exp),
                );
            }
            shown += 1;
        }
    }

    tally.merge(&total);
    Ok(())
}

fn sorted<T: Ord>(mut v: Vec<T>) -> Vec<T> {
    v.sort();
    v
}

fn main() -> Result<()> {
    let args = parse_args();
    println!("fixtures root: {}", args.fixtures.display());
    if !args.filters.is_empty() {
        println!("filters:       {}", args.filters.join(", "));
    }
    if args.full_history {
        println!("mode:          full-history (multi-rev replay)");
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
    let mut tally = Tally {
        full_history: args.full_history,
        ..Tally::default()
    };
    for fx in &fixtures {
        let result = if args.full_history {
            process_one_full_history(fx, &args, &mut tally)
        } else {
            process_one(fx, &args, &mut tally)
        };
        if let Err(e) = result {
            eprintln!("  SKIP {}: {:#}", fx.display(), e);
            tally.add_load_failure();
        }
    }
    tally.report(started.elapsed().as_millis());
    Ok(())
}
