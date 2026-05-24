//! Run the wikitext-injection parser on the real Icaro wikitext +
//! the real token list captured from our deployed server. Writes
//! the modified wikitext to /tmp/icaro_modified.wt so we can
//! diff against the original and find what's leaking inside
//! `{{multiple issues|...}}`.
//!
//! Skips if the input files aren't present. Not part of CI; only
//! runs when the operator explicitly invokes it.

use serde::Deserialize;
use std::path::Path;
use wikiwho_server::whocolor_wikitext::{
    inject_spans_into_wikitext, WikitextToken,
};

#[derive(Deserialize)]
struct RawToken {
    #[serde(rename = "str")]
    s: String,
    editor: String,
    class_name: String,
}

/// Known-failing repro of the en/Icaro `{{multiple issues}}` bug.
/// Filed in notes/decisions-needed.md (2026-05-24 entry). Run via
/// `cargo test -p wikiwho-server --test icaro_repro -- --ignored`
/// once /tmp/icaro.wt and /tmp/icaro_tokens.json have been captured
/// (see the comments at the top of this file).
#[test]
#[ignore]
fn icaro_real_data() {
    let wt_path = Path::new("/tmp/icaro.wt");
    let tk_path = Path::new("/tmp/icaro_tokens.json");
    if !wt_path.exists() || !tk_path.exists() {
        eprintln!("skipping: /tmp/icaro.wt or /tmp/icaro_tokens.json missing");
        return;
    }
    let wikitext = std::fs::read_to_string(wt_path).unwrap();
    let raw_tokens: Vec<RawToken> =
        serde_json::from_str(&std::fs::read_to_string(tk_path).unwrap()).unwrap();
    let tokens: Vec<WikitextToken> = raw_tokens
        .into_iter()
        .map(|r| WikitextToken {
            str: r.s,
            editor: r.editor,
            class_name: r.class_name,
        })
        .collect();
    let out = inject_spans_into_wikitext(&wikitext, &tokens);
    std::fs::write("/tmp/icaro_modified.wt", &out.wikitext).unwrap();
    eprintln!("wrote /tmp/icaro_modified.wt ({} bytes)", out.wikitext.len());

    // Find {{multiple issues...}} in the modified output. Assert no
    // span markup inside it.
    let mi_start = out.wikitext.find("{{multiple issues").expect("output has {{multiple issues");
    // The outer template's closing `}}}}` (4 braces, 2 closes). After
    // it, the next line begins with `{{other}}`. Look for `{{other`.
    let after_close = out.wikitext.find("{{other").expect("output has {{other after multiple issues");
    let body = &out.wikitext[mi_start..after_close];
    let span_count = body.matches("<span class=\"editor-token").count();
    eprintln!("--- {{{{multiple issues}}}} body (len {}, {} spans inside) ---", body.len(), span_count);
    eprintln!("{}", body);
    assert_eq!(
        span_count, 0,
        "no spans should appear inside {{{{multiple issues...}}}}"
    );
}
