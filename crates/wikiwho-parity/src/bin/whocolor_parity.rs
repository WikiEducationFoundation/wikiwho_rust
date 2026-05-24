//! `whocolor-parity` — compares the rewrite's WhoColor data shape to
//! captured production WhoColor responses (`whocolor.json` under each
//! fixture directory).
//!
//! What we compare (algorithm-level fields only):
//!
//! - `biggest_conflict_score` — scalar
//! - `tokens` — per-position match on `(str, o_rev_id, in, out,
//!   conflict_score, class_name)`. `age` is excluded by default since
//!   it's `(capture_time - origin_time)` and depends on when production
//!   captured the response; with `--check-age` we infer the capture
//!   time from production's own `age` + revision timestamps and verify
//!   our output matches.
//! - `revisions` dict — by `rev_id`, compares
//!   `(timestamp, parent_rev_id, class_name)`. Editor *name* is
//!   excluded (requires live MW resolution).
//!
//! What we do NOT compare:
//!
//! - `extended_html` — production uses MW's `action=parse` over
//!   wikitext-annotated-with-spans. The rewrite uses Parsoid REST
//!   (`/api/rest_v1/page/html/...`) and injects spans HTML-side per
//!   PLAN.md §4.6 Option A. The output structure is fundamentally
//!   different.
//! - `present_editors` — production counts editors via the
//!   wikitext-side parser, which sees tokens (like `[[`, `#`,
//!   template names) that don't exist in the rendered HTML. Our
//!   HTML-side counter naturally has fewer entries; the gap is
//!   expected, not a bug.
//!
//! Usage:
//!   whocolor-parity                       # all fixtures vs prod whocolor.json
//!   whocolor-parity zh/1686258            # one fixture by lang/page_id
//!   whocolor-parity --fixtures path/to/   # alternative root
//!   whocolor-parity --check-age           # also compare per-token age (prod mode)
//!   whocolor-parity --show-first-diff     # print first per-fixture divergence
//!   whocolor-parity --python-replay       # compare vs a fresh Python wikiwho.py replay
//!   whocolor-parity --refresh-python      # force regenerate python_whocolor_replay.json
//!
//! With `--python-replay`, the reference is `scripts/python_replay.py
//! --whocolor`'s output (cached at `<fixture>/python_whocolor_replay.
//! json`) instead of `whocolor.json`. That removes the prod-cache-
//! drift confound documented in `notes/2026-05-23-whocolor-parity.md`:
//! ja/4821051 and en/62750956 fail vs prod but are 100% vs Python. The
//! `age` field is not emitted by `--whocolor` (it depends on Python's
//! `datetime.now()` at script-invocation time), so `--check-age` is a
//! no-op in python-replay mode.
//!
//! Each fixture reports `tokens_pass/total`, `revisions_pass/total`,
//! and the per-field exact-match score. Exit non-zero if any
//! mandatory field (biggest_conflict_score, token shape) diverges on
//! any fixture.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use wikiwho_attribute::pipeline::RevisionInput;
use wikiwho_attribute::structures::Article;
use wikiwho_attribute::whocolor::{WhoColorData, get_whocolor_data};

// ---------------- fixture loading ----------------

#[derive(Debug, Deserialize)]
struct Meta {
    #[allow(dead_code)]
    lang: String,
    title: String,
    page_id: u64,
    rev_id: u64,
}

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

/// Parsed view of the comparison source — either production's
/// captured `whocolor.json` or `scripts/python_replay.py --whocolor`'s
/// output. Both share the same wire shape for `tokens` / `revisions` /
/// `biggest_conflict_score`. Python mode skips the `age` element on
/// each token tuple and sets `success`/`rev_id` synthetically.
#[derive(Debug, Deserialize)]
struct ComparisonSource {
    #[serde(default = "default_true")]
    success: bool,
    rev_id: u64,
    biggest_conflict_score: u32,
    /// `[conflict_score, str, o_rev_id, in, out, class_name, age]`
    /// per API.md §7 (production mode) or the same minus `age` in
    /// python-replay mode. We model this as `serde_json::Value`
    /// because the tuple's last element is a float and the rest are
    /// mixed.
    tokens: Vec<serde_json::Value>,
    /// `rev_id_string → [timestamp, parent_id, class_name, editor_name]`.
    revisions: BTreeMap<String, Vec<serde_json::Value>>,
}

fn default_true() -> bool { true }

/// Wrapper for the python_replay.py output shape — only used to extract
/// the `whocolor` sub-object before reshaping it into a
/// [`ComparisonSource`]. The rest of the fields (spam_ids, token_count,
/// etc.) are useful diagnostics but not consumed here.
#[derive(Debug, Deserialize)]
struct PythonReplayEnvelope {
    target_rev_id: u64,
    whocolor: Option<PythonWhocolor>,
}

#[derive(Debug, Deserialize)]
struct PythonWhocolor {
    biggest_conflict_score: u32,
    tokens: Vec<serde_json::Value>,
    revisions: BTreeMap<String, Vec<serde_json::Value>>,
}

fn default_fixtures_root() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    Path::new(manifest_dir)
        .join("..")
        .join("..")
        .join("parity-fixtures")
        .canonicalize()
        .unwrap_or_else(|_| Path::new(manifest_dir).join("../../parity-fixtures"))
}

fn walk_fixtures(
    root: &Path,
    filters: &[String],
    python_mode: bool,
) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let lang_dirs = fs::read_dir(root)
        .with_context(|| format!("reading {}", root.display()))?;
    for lang in lang_dirs {
        let lang = lang?;
        if !lang.path().is_dir() {
            continue;
        }
        for page in fs::read_dir(lang.path())? {
            let page = page?;
            if !page.path().is_dir() {
                continue;
            }
            for rev in fs::read_dir(page.path())? {
                let rev = rev?;
                if !rev.path().is_dir() {
                    continue;
                }
                let dir = rev.path();
                // Both modes need history + meta; only prod mode needs
                // the captured whocolor.json (python mode generates its
                // own reference via the replay script).
                if !dir.join("history.jsonl").exists()
                    || !dir.join("meta.json").exists()
                {
                    continue;
                }
                if !python_mode && !dir.join("whocolor.json").exists() {
                    continue;
                }
                let rel: String = dir
                    .strip_prefix(root)
                    .unwrap()
                    .display()
                    .to_string();
                if filters.is_empty()
                    || filters.iter().any(|f| rel.starts_with(f))
                {
                    out.push(dir);
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

// ---------------- comparison ----------------

#[derive(Default, Clone, Copy)]
struct TokenScore {
    str: bool,
    o_rev_id: bool,
    inbound: bool,
    outbound: bool,
    conflict_score: bool,
    class_name: bool,
    age: bool,
}

impl TokenScore {
    fn all_required_pass(self) -> bool {
        // age is optional (controlled by --check-age)
        self.str && self.o_rev_id && self.inbound && self.outbound
            && self.conflict_score && self.class_name
    }
}

#[derive(Default)]
struct FixtureMetrics {
    tokens_total: usize,
    tokens_pass: usize,
    field_str_pass: usize,
    field_o_rev_id_pass: usize,
    field_in_pass: usize,
    field_out_pass: usize,
    field_conflict_pass: usize,
    field_class_pass: usize,
    field_age_pass: usize,
    age_checked: usize,
    revisions_total: usize,
    revisions_pass: usize,
    biggest_conflict_matches: bool,
    first_token_diff: Option<String>,
    first_rev_diff: Option<String>,
}

fn compare_tokens(
    rust: &[wikiwho_attribute::whocolor::WhoColorToken],
    prod: &[serde_json::Value],
    check_age: bool,
    metrics: &mut FixtureMetrics,
) {
    let n = rust.len().min(prod.len());
    metrics.tokens_total = prod.len();
    if rust.len() != prod.len() {
        metrics.first_token_diff = Some(format!(
            "token count differs: rust={} expected={}",
            rust.len(),
            prod.len()
        ));
    }
    for i in 0..n {
        let rt = &rust[i];
        let pt = prod[i].as_array().expect("prod token is array");
        let score = score_one_token(rt, pt, check_age);
        if score.str { metrics.field_str_pass += 1; }
        if score.o_rev_id { metrics.field_o_rev_id_pass += 1; }
        if score.inbound { metrics.field_in_pass += 1; }
        if score.outbound { metrics.field_out_pass += 1; }
        if score.conflict_score { metrics.field_conflict_pass += 1; }
        if score.class_name { metrics.field_class_pass += 1; }
        if check_age {
            metrics.age_checked += 1;
            if score.age { metrics.field_age_pass += 1; }
        }
        if score.all_required_pass() {
            metrics.tokens_pass += 1;
        } else if metrics.first_token_diff.is_none() {
            metrics.first_token_diff = Some(format_token_diff(i, rt, pt, score));
        }
    }
}

fn score_one_token(
    rt: &wikiwho_attribute::whocolor::WhoColorToken,
    pt: &[serde_json::Value],
    check_age: bool,
) -> TokenScore {
    // pt = [conflict_score, str, o_rev_id, in, out, class_name, age]
    let class_name_rust = wikiwho_server::whocolor_html::token_class_name(&rt.editor);
    let conflict_score = pt
        .first()
        .and_then(|v| v.as_u64())
        .map(|c| c as u32 == rt.conflict_score)
        .unwrap_or(false);
    let str = pt
        .get(1)
        .and_then(|v| v.as_str())
        .map(|c| c == rt.str)
        .unwrap_or(false);
    let o_rev_id = pt
        .get(2)
        .and_then(|v| v.as_u64())
        .map(|c| c == rt.o_rev_id)
        .unwrap_or(false);
    let inbound = pt
        .get(3)
        .and_then(|v| v.as_array())
        .map(|arr| {
            let prod_in: Vec<u64> = arr.iter().filter_map(|v| v.as_u64()).collect();
            prod_in == rt.inbound
        })
        .unwrap_or(false);
    let outbound = pt
        .get(4)
        .and_then(|v| v.as_array())
        .map(|arr| {
            let prod_out: Vec<u64> = arr.iter().filter_map(|v| v.as_u64()).collect();
            prod_out == rt.outbound
        })
        .unwrap_or(false);
    let class_name = pt
        .get(5)
        .and_then(|v| v.as_str())
        .map(|c| c == class_name_rust)
        .unwrap_or(false);
    let age = if check_age {
        pt.get(6)
            .and_then(|v| v.as_f64())
            .map(|c| (c - rt.age_seconds).abs() < 1.5)
            .unwrap_or(false)
    } else {
        false
    };
    TokenScore {
        str,
        o_rev_id,
        inbound,
        outbound,
        conflict_score,
        class_name,
        age,
    }
}

fn format_token_diff(
    i: usize,
    rt: &wikiwho_attribute::whocolor::WhoColorToken,
    pt: &[serde_json::Value],
    score: TokenScore,
) -> String {
    let mut bad: Vec<String> = Vec::new();
    if !score.conflict_score {
        bad.push(format!(
            "conflict_score: rust={} expected={}",
            rt.conflict_score,
            pt.first().and_then(|v| v.as_u64()).unwrap_or(0),
        ));
    }
    if !score.str {
        bad.push(format!(
            "str: rust={:?} expected={:?}",
            rt.str,
            pt.get(1).and_then(|v| v.as_str()).unwrap_or(""),
        ));
    }
    if !score.o_rev_id {
        bad.push(format!(
            "o_rev_id: rust={} expected={}",
            rt.o_rev_id,
            pt.get(2).and_then(|v| v.as_u64()).unwrap_or(0),
        ));
    }
    if !score.inbound {
        let prod: Vec<u64> = pt
            .get(3)
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_u64()).collect())
            .unwrap_or_default();
        bad.push(format!("in: rust={:?} expected={:?}", rt.inbound, prod));
    }
    if !score.outbound {
        let prod: Vec<u64> = pt
            .get(4)
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_u64()).collect())
            .unwrap_or_default();
        bad.push(format!("out: rust={:?} expected={:?}", rt.outbound, prod));
    }
    if !score.class_name {
        bad.push(format!(
            "class_name: rust={} expected={:?}",
            wikiwho_server::whocolor_html::token_class_name(&rt.editor),
            pt.get(5).and_then(|v| v.as_str()).unwrap_or(""),
        ));
    }
    format!("token[{i}] ({}): {}", rt.str, bad.join("; "))
}

fn compare_revisions(
    rust: &WhoColorData,
    prod_revisions: &BTreeMap<String, Vec<serde_json::Value>>,
    metrics: &mut FixtureMetrics,
) {
    metrics.revisions_total = prod_revisions.len();
    for (rid, rev) in &rust.revisions {
        let rid_key = rid.to_string();
        let Some(prod_entry) = prod_revisions.get(&rid_key) else {
            if metrics.first_rev_diff.is_none() {
                metrics.first_rev_diff = Some(format!("rust rev {rid} absent from expected"));
            }
            continue;
        };
        let class_name_rust =
            wikiwho_server::whocolor_html::token_class_name(&rev.editor);
        let ts_match = prod_entry
            .first()
            .and_then(|v| v.as_str())
            .map(|s| s == rev.timestamp)
            .unwrap_or(false);
        let parent_match = prod_entry
            .get(1)
            .and_then(|v| v.as_u64())
            .map(|p| p == rev.parent_rev_id)
            .unwrap_or(false);
        let class_match = prod_entry
            .get(2)
            .and_then(|v| v.as_str())
            .map(|c| c == class_name_rust)
            .unwrap_or(false);
        if ts_match && parent_match && class_match {
            metrics.revisions_pass += 1;
        } else if metrics.first_rev_diff.is_none() {
            metrics.first_rev_diff = Some(format!(
                "rev {rid}: ts_match={ts_match} parent_match={parent_match} class_match={class_match}; \
                rust=({}, {}, {}) expected=({:?}, {:?}, {:?})",
                rev.timestamp,
                rev.parent_rev_id,
                class_name_rust,
                prod_entry.first().and_then(|v| v.as_str()).unwrap_or(""),
                prod_entry.get(1).and_then(|v| v.as_u64()).unwrap_or(0),
                prod_entry.get(2).and_then(|v| v.as_str()).unwrap_or(""),
            ));
        }
    }
}

/// Infer the production capture time from prod's `age` values and our
/// just-built `revisions`'s `timestamp`s. Production records
/// `age = capture_now - origin_timestamp`, so
/// `capture_now ≈ age + origin_timestamp_unix`. We average across the
/// first ~10 tokens to dampen rounding noise.
fn infer_capture_now(rust: &WhoColorData, prod: &ComparisonSource) -> Option<i64> {
    let mut samples: Vec<f64> = Vec::new();
    let n = rust.tokens.len().min(prod.tokens.len()).min(10);
    for i in 0..n {
        let rt = &rust.tokens[i];
        let pt = prod.tokens[i].as_array()?;
        let prod_age = pt.get(6)?.as_f64()?;
        // origin timestamp from our revisions list
        let origin_ts = rust
            .revisions
            .iter()
            .find(|(rid, _)| *rid == rt.o_rev_id)
            .map(|(_, rev)| rev.timestamp.as_str())?;
        let origin_unix = wikiwho_attribute::whocolor::parse_mw_timestamp_public(origin_ts)?;
        samples.push(origin_unix as f64 + prod_age);
    }
    if samples.is_empty() {
        return None;
    }
    let avg = samples.iter().sum::<f64>() / samples.len() as f64;
    Some(avg.round() as i64)
}

// ---------------- CLI ----------------

struct Args {
    fixtures: PathBuf,
    filters: Vec<String>,
    check_age: bool,
    show_first_diff: bool,
    /// Compare against a fresh Python wikiwho.py replay (cached at
    /// `<fixture>/python_whocolor_replay.json`) instead of the captured
    /// production `whocolor.json`. Eliminates prod-cache drift.
    python_replay: bool,
    /// Force re-running the Python reference even if a cached
    /// `python_whocolor_replay.json` exists. Implies `--python-replay`.
    refresh_python: bool,
}

fn parse_args() -> Args {
    let mut args = std::env::args().skip(1);
    let mut out = Args {
        fixtures: default_fixtures_root(),
        filters: Vec::new(),
        check_age: false,
        show_first_diff: false,
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
            "--check-age" => out.check_age = true,
            "--show-first-diff" => out.show_first_diff = true,
            "--python-replay" => out.python_replay = true,
            "--refresh-python" => {
                out.python_replay = true;
                out.refresh_python = true;
            }
            "-h" | "--help" => {
                eprintln!("{}", env!("CARGO_PKG_DESCRIPTION"));
                eprintln!();
                eprintln!(
                    "Usage: whocolor-parity [--fixtures DIR] [--check-age] \
                     [--show-first-diff] [--python-replay] [--refresh-python] \
                     [LANG/PAGE_ID ...]"
                );
                std::process::exit(0);
            }
            other => out.filters.push(other.to_string()),
        }
    }
    out
}

// ---------------- per-fixture driver ----------------

/// Run `scripts/python_replay.py --whocolor` if the cache is missing
/// or `--refresh-python` was set. Cache lives at
/// `<fixture>/python_whocolor_replay.json` and is regeneratable from
/// `history.jsonl` alone.
fn load_python_whocolor(
    fixture: &Path,
    meta: &Meta,
    refresh: bool,
) -> Result<ComparisonSource> {
    let cache = fixture.join("python_whocolor_replay.json");
    if !cache.exists() || refresh {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("scripts")
            .join("python_replay.py");
        eprintln!(
            "  [python] regenerating {} via {}",
            cache.display(),
            script.display(),
        );
        let output = std::process::Command::new("python3")
            .arg(&script)
            .arg(fixture)
            .arg("--whocolor")
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
    let envelope: PythonReplayEnvelope = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {}", cache.display()))?;
    if envelope.target_rev_id != meta.rev_id {
        bail!(
            "{}: python_whocolor_replay.target_rev_id={} disagrees with meta.rev_id={}",
            meta.title,
            envelope.target_rev_id,
            meta.rev_id,
        );
    }
    let wc = envelope.whocolor.ok_or_else(|| {
        anyhow::anyhow!(
            "{}: python_whocolor_replay has whocolor=null — Python flagged \
             target rev as spam or unreachable",
            meta.title,
        )
    })?;
    Ok(ComparisonSource {
        success: true,
        rev_id: meta.rev_id,
        biggest_conflict_score: wc.biggest_conflict_score,
        tokens: wc.tokens,
        revisions: wc.revisions,
    })
}

fn process_one(fixture: &Path, args: &Args) -> Result<FixtureMetrics> {
    let meta: Meta = serde_json::from_str(
        &fs::read_to_string(fixture.join("meta.json"))?,
    )?;
    let source: ComparisonSource = if args.python_replay {
        load_python_whocolor(fixture, &meta, args.refresh_python)?
    } else {
        let s: ComparisonSource =
            serde_json::from_str(&fs::read_to_string(fixture.join("whocolor.json"))?)?;
        if !s.success {
            bail!("{}: whocolor.json success=false; refusing to parity-check", meta.title);
        }
        if s.rev_id != meta.rev_id {
            bail!(
                "{}: meta.rev_id={} disagrees with whocolor.rev_id={}",
                meta.title,
                meta.rev_id,
                s.rev_id,
            );
        }
        s
    };

    let history_text = fs::read_to_string(fixture.join("history.jsonl"))?;
    let entries: Vec<HistoryEntry> = history_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;

    let mut article = Article::new(&meta.title);
    article.page_id = Some(meta.page_id);
    for entry in &entries {
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

    // Probe at capture_now=0 to get deterministic fields; recompute with
    // the inferred capture_now only when --check-age is on AND the source
    // has age data to infer from (prod whocolor.json does, python_replay
    // doesn't).
    let probe = get_whocolor_data(&article, meta.rev_id, 0)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let check_age = args.check_age && !args.python_replay;
    let rust_data = if check_age {
        let capture_now = infer_capture_now(&probe, &source).unwrap_or(0);
        get_whocolor_data(&article, meta.rev_id, capture_now)
            .map_err(|e| anyhow::anyhow!("{e}"))?
    } else {
        probe
    };

    let mut m = FixtureMetrics {
        biggest_conflict_matches: rust_data.biggest_conflict_score
            == source.biggest_conflict_score,
        ..Default::default()
    };
    compare_tokens(&rust_data.tokens, &source.tokens, check_age, &mut m);
    compare_revisions(&rust_data, &source.revisions, &mut m);
    Ok(m)
}

fn pct(numer: usize, denom: usize) -> f64 {
    if denom == 0 { 0.0 } else { (numer as f64 / denom as f64) * 100.0 }
}

fn main() -> Result<()> {
    let args = parse_args();
    let fixtures = walk_fixtures(&args.fixtures, &args.filters, args.python_replay)?;
    if fixtures.is_empty() {
        eprintln!("no whocolor fixtures matched filters");
        std::process::exit(1);
    }

    let mut total_tokens = 0usize;
    let mut total_tokens_pass = 0usize;
    let mut total_revs = 0usize;
    let mut total_revs_pass = 0usize;
    let mut bc_passes = 0usize;
    let mut bc_total = 0usize;
    let mut any_mandatory_fail = false;

    let mut field_str = 0usize;
    let mut field_o = 0usize;
    let mut field_in = 0usize;
    let mut field_out = 0usize;
    let mut field_c = 0usize;
    let mut field_cls = 0usize;
    let mut field_age = 0usize;
    let mut age_checked = 0usize;

    let source_label = if args.python_replay { "vs python" } else { "vs prod-cache" };
    println!(
        "Whocolor parity ({} fixtures, {}, age check {})",
        fixtures.len(),
        source_label,
        if args.check_age && !args.python_replay { "ON" } else { "off" },
    );
    println!();
    println!(
        "{:<32} {:>10} {:>10} {:>8} {:>4}",
        "fixture", "tokens", "revs", "biggest", "ok?",
    );
    for fixture in &fixtures {
        let rel = fixture
            .strip_prefix(&args.fixtures)
            .unwrap()
            .display()
            .to_string();
        let m = match process_one(fixture, &args) {
            Ok(m) => m,
            Err(e) => {
                println!("{rel:<32} ERROR: {e}");
                any_mandatory_fail = true;
                continue;
            }
        };
        total_tokens += m.tokens_total;
        total_tokens_pass += m.tokens_pass;
        total_revs += m.revisions_total;
        total_revs_pass += m.revisions_pass;
        bc_total += 1;
        if m.biggest_conflict_matches { bc_passes += 1; }
        field_str += m.field_str_pass;
        field_o += m.field_o_rev_id_pass;
        field_in += m.field_in_pass;
        field_out += m.field_out_pass;
        field_c += m.field_conflict_pass;
        field_cls += m.field_class_pass;
        field_age += m.field_age_pass;
        age_checked += m.age_checked;
        let all_pass = m.biggest_conflict_matches
            && m.tokens_pass == m.tokens_total
            && m.revisions_pass == m.revisions_total;
        if !all_pass {
            any_mandatory_fail = true;
        }
        println!(
            "{rel:<32} {tp}/{tt} ({:>5.1}%) {rp}/{rt} ({:>5.1}%) {bc:>8} {ok:>4}",
            pct(m.tokens_pass, m.tokens_total),
            pct(m.revisions_pass, m.revisions_total),
            bc = if m.biggest_conflict_matches { "✓" } else { "✗" },
            ok = if all_pass { "✓" } else { "✗" },
            tp = m.tokens_pass,
            tt = m.tokens_total,
            rp = m.revisions_pass,
            rt = m.revisions_total,
        );
        if args.show_first_diff {
            if let Some(t) = &m.first_token_diff {
                println!("   token-diff: {t}");
            }
            if let Some(r) = &m.first_rev_diff {
                println!("   rev-diff: {r}");
            }
        }
    }

    println!();
    println!("=== Aggregate ===");
    println!(
        "tokens passing (all required fields): {}/{} ({:.2}%)",
        total_tokens_pass,
        total_tokens,
        pct(total_tokens_pass, total_tokens),
    );
    println!("  str        : {}/{} ({:.2}%)", field_str, total_tokens, pct(field_str, total_tokens));
    println!("  o_rev_id   : {}/{} ({:.2}%)", field_o, total_tokens, pct(field_o, total_tokens));
    println!("  in         : {}/{} ({:.2}%)", field_in, total_tokens, pct(field_in, total_tokens));
    println!("  out        : {}/{} ({:.2}%)", field_out, total_tokens, pct(field_out, total_tokens));
    println!("  conflict   : {}/{} ({:.2}%)", field_c, total_tokens, pct(field_c, total_tokens));
    println!("  class_name : {}/{} ({:.2}%)", field_cls, total_tokens, pct(field_cls, total_tokens));
    if args.check_age && age_checked > 0 {
        println!("  age (±1.5s): {}/{} ({:.2}%)", field_age, age_checked, pct(field_age, age_checked));
    }
    println!(
        "revisions passing: {}/{} ({:.2}%)",
        total_revs_pass,
        total_revs,
        pct(total_revs_pass, total_revs),
    );
    println!(
        "biggest_conflict_score matches: {}/{}",
        bc_passes, bc_total,
    );

    if any_mandatory_fail {
        std::process::exit(1);
    }
    Ok(())
}
