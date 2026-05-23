//! Parity test against Python's `difflib.Differ`. Runs a Python
//! subprocess (`scripts/verify_differ.py`) over a fixed set of test
//! inputs and asserts the Rust port produces an identical sequence of
//! `(tag, value)` pairs for every case.
//!
//! Skipped when `python3` is absent. Re-run after any change to
//! `differ.rs` — a regression here means the cascade will start
//! attributing tokens differently than Python.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use wikiwho_attribute::differ::{differ_compare, DiffOp};

fn workspace_root() -> PathBuf {
    // The tests binary runs from .../wikiwho_rust/target/debug/deps;
    // CARGO_MANIFEST_DIR points at this crate's dir, so go up two
    // levels to reach the workspace root.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap().parent().unwrap().to_path_buf()
}

fn python_filter(cases: &[(Vec<String>, Vec<String>)]) -> Option<Vec<Vec<DiffOp>>> {
    let script = workspace_root().join("scripts").join("verify_differ.py");
    if !script.exists() {
        eprintln!("verify_differ.py not found at {script:?}; skipping");
        return None;
    }

    let json_in = serde_json::to_string(
        &cases
            .iter()
            .map(|(a, b)| (a.clone(), b.clone()))
            .collect::<Vec<_>>(),
    )
    .expect("input cases serialize");

    let mut child = match Command::new("python3")
        .arg(&script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("python3 not available ({e}); skipping parity test");
            return None;
        }
    };
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(json_in.as_bytes())
        .unwrap();
    let output = child.wait_with_output().expect("python3 finishes");
    assert!(
        output.status.success(),
        "verify_differ.py exited non-zero: {:?}",
        output.status
    );

    let raw: Vec<Vec<(String, String)>> = serde_json::from_slice(&output.stdout).expect(
        "verify_differ.py output is well-formed JSON [[(tag, value), ...], ...]",
    );

    let parsed: Vec<Vec<DiffOp>> = raw
        .into_iter()
        .map(|case| {
            case.into_iter()
                .map(|(tag, value)| match tag.as_str() {
                    "keep" => DiffOp::Keep(value),
                    "delete" => DiffOp::Delete(value),
                    "insert" => DiffOp::Insert(value),
                    _ => panic!("unknown tag {tag}"),
                })
                .collect()
        })
        .collect();

    Some(parsed)
}

/// Hardcoded test cases. Each case is `(text_prev, text_curr)`; we
/// pass them through both Python's `Differ.compare()` (filtered) and
/// our `differ_compare` and check the sequences match.
fn cases() -> Vec<(Vec<String>, Vec<String>)> {
    let to_v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let words = |s: &str| {
        s.split_whitespace()
            .map(String::from)
            .collect::<Vec<String>>()
    };

    // Long sequence with a popular token (triggers autojunk on the
    // outer SequenceMatcher, n >= 200).
    let (autojunk_prev, autojunk_curr) = {
        let mut prev = Vec::new();
        let mut curr = Vec::new();
        for i in 0..250 {
            if i % 5 == 0 {
                prev.push("the".to_string());
                curr.push("the".to_string());
            } else {
                prev.push(format!("w{i}"));
                curr.push(if i % 7 == 0 {
                    format!("x{i}")
                } else {
                    format!("w{i}")
                });
            }
        }
        (prev, curr)
    };

    vec![
        // Empty / pure inserts / pure deletes / identical.
        (to_v(&[]), to_v(&[])),
        (to_v(&[]), to_v(&["a", "b", "c"])),
        (to_v(&["a", "b", "c"]), to_v(&[])),
        (to_v(&["foo", "bar", "baz"]), to_v(&["foo", "bar", "baz"])),
        // Single substitution / pure insertion / pure deletion in middle.
        (to_v(&["foo", "bar", "baz"]), to_v(&["foo", "qux", "baz"])),
        (to_v(&["a", "c"]), to_v(&["a", "b", "c"])),
        (to_v(&["a", "b", "c"]), to_v(&["a", "c"])),
        // _fancy_replace cases (close-but-not-identical tokens).
        (to_v(&["hello", "world"]), to_v(&["hello", "worle"])),
        (to_v(&["hello"]), to_v(&["world", "hella"])),
        // Duplicate tokens / transpositions / reversals.
        (
            to_v(&["the", "cat", "the", "rat"]),
            to_v(&["the", "dog", "the", "rat"]),
        ),
        (to_v(&["a", "b"]), to_v(&["x", "y", "z"])),
        (to_v(&["a", "b", "c"]), to_v(&["x", "y"])),
        (to_v(&["a", "b", "c", "d"]), to_v(&["a", "c", "b", "d"])),
        (to_v(&["foo", "bar"]), to_v(&["bar", "foo"])),
        (to_v(&["1", "2", "3"]), to_v(&["3", "2", "1"])),
        // Close-match patterns triggering _fancy_replace.
        (
            to_v(&["foobar", "qux", "abcdef"]),
            to_v(&["fewbar", "zzz", "ghijkl"]),
        ),
        (
            words("The quick brown fox jumps over the lazy dog ."),
            words("The quick brown fox jumps over the lazy cat ."),
        ),
        // Vandalism-and-revert flavoured.
        (
            words("alpha beta gamma delta epsilon"),
            vec!["spam".to_string()],
        ),
        (
            vec!["spam".to_string()],
            words("alpha beta gamma delta epsilon"),
        ),
        // Autojunk territory.
        (autojunk_prev, autojunk_curr),
        // Wide replace block with no close matches.
        (
            to_v(&["aa", "bb", "cc", "dd"]),
            to_v(&["xxx", "yyy", "zzz", "www", "vvv"]),
        ),
        // Close-pair adjacent to unique deletes — exercises
        // _fancy_replace's recursive helper.
        (to_v(&["xxx", "abcde", "yyy", "zzz"]), to_v(&["abcdf"])),
    ]
}

#[test]
fn differ_matches_python_on_curated_cases() {
    let cases = cases();
    let Some(python_out) = python_filter(&cases) else {
        return;
    };

    let mut failures: Vec<String> = Vec::new();
    for (i, ((prev, curr), expected)) in cases.iter().zip(python_out.iter()).enumerate() {
        let actual = differ_compare(prev, curr);
        if &actual != expected {
            failures.push(format!(
                "case #{i}: prev={prev:?} curr={curr:?}\n  expected: {expected:?}\n  actual:   {actual:?}"
            ));
        }
    }
    if !failures.is_empty() {
        panic!("{} parity mismatches:\n{}", failures.len(), failures.join("\n"));
    }
}
