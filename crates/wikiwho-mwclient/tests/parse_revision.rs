//! Unit tests for `wikiwho_mwclient::revisions::parse_revision`.
//!
//! Strategy:
//!   1. Hand-crafted MW Action API revision JSON objects covering the
//!      formatversion=2 quirks we know about (minor present-as-bool,
//!      texthidden/userhidden/commenthidden, missing parentid, slots
//!      vs top-level `*`).
//!   2. Round-trip: existing `parity-fixtures/*/history.jsonl` rows
//!      (produced by the Python script) deserialize cleanly into
//!      `Revision` and re-serialize identically. This is the
//!      compatibility test that lets us deprecate the Python script.

use serde_json::json;
use wikiwho_mwclient::Revision;
use wikiwho_mwclient::revisions::parse_revision;

#[test]
fn fv2_typical_revision() {
    let rev = json!({
        "revid": 1083758908,
        "parentid": 1083000000,
        "user": "Sichoon",
        "userid": 33041991,
        "timestamp": "2022-04-20T14:42:55Z",
        "minor": false,
        "sha1": "fe56f6f3045d118355c235071be9b63431172ec1",
        "comment": "describing an organocatalyst",
        "slots": {"main": {"contentmodel": "wikitext", "content": "Hello world."}}
    });
    let r = parse_revision(&rev).unwrap();
    assert_eq!(r.rev_id, 1083758908);
    assert_eq!(r.parent_id, 1083000000);
    assert_eq!(r.user_name.as_deref(), Some("Sichoon"));
    assert_eq!(r.user_id, Some(33041991));
    assert!(!r.minor);
    assert_eq!(r.text, "Hello world.");
    assert!(!r.text_hidden);
    assert_eq!(r.sha1.as_deref(), Some("fe56f6f3045d118355c235071be9b63431172ec1"));
}

#[test]
fn fv2_first_revision_has_no_parentid() {
    // formatversion=2 still emits parentid=0 for the first revision; the
    // capture script normalizes a missing parentid to 0 too, since older
    // captures may have omitted the field.
    let rev = json!({
        "revid": 17942981,
        "timestamp": "2011-10-05T06:35:56Z",
        "minor": true,
        "slots": {"main": {"content": "Hello"}}
    });
    let r = parse_revision(&rev).unwrap();
    assert_eq!(r.parent_id, 0);
    assert!(r.minor);
}

#[test]
fn texthidden_yields_empty_text() {
    // texthidden lives under slots.main.texthidden in fv=2. Confirm we
    // detect it and produce empty `text` rather than panic on missing
    // content.
    let rev = json!({
        "revid": 999,
        "parentid": 998,
        "timestamp": "2024-01-01T00:00:00Z",
        "minor": false,
        "slots": {"main": {"texthidden": true}}
    });
    let r = parse_revision(&rev).unwrap();
    assert!(r.text_hidden);
    assert_eq!(r.text, "");
}

#[test]
fn userhidden_nullifies_user_fields() {
    let rev = json!({
        "revid": 1000,
        "parentid": 0,
        "timestamp": "2024-01-01T00:00:00Z",
        "minor": false,
        "userhidden": "",
        "user": "redacted",
        "userid": 0,
        "slots": {"main": {"content": "x"}}
    });
    let r = parse_revision(&rev).unwrap();
    assert_eq!(r.user_id, None);
    assert_eq!(r.user_name, None);
}

#[test]
fn commenthidden_nullifies_comment() {
    let rev = json!({
        "revid": 1001,
        "parentid": 0,
        "timestamp": "2024-01-01T00:00:00Z",
        "minor": false,
        "commenthidden": "",
        "comment": "redacted",
        "slots": {"main": {"content": "x"}}
    });
    let r = parse_revision(&rev).unwrap();
    assert_eq!(r.comment, None);
}

#[test]
fn suppressed_implies_text_hidden_and_comment_hidden() {
    let rev = json!({
        "revid": 1002,
        "parentid": 0,
        "timestamp": "2024-01-01T00:00:00Z",
        "minor": false,
        "suppressed": "",
        "slots": {"main": {"content": "should-not-leak"}}
    });
    let r = parse_revision(&rev).unwrap();
    assert!(r.text_hidden);
    assert_eq!(r.text, "");
    assert_eq!(r.comment, None);
}

#[test]
fn legacy_star_field_falls_back_when_no_slots() {
    // Older MW responses (no rvslots) put content under `*`. The
    // Action API still emits these on some wikis; tolerate both.
    let rev = json!({
        "revid": 1003,
        "parentid": 1002,
        "timestamp": "2010-01-01T00:00:00Z",
        "minor": false,
        "*": "legacy text"
    });
    let r = parse_revision(&rev).unwrap();
    assert_eq!(r.text, "legacy text");
    assert!(!r.text_hidden);
}

#[test]
fn round_trip_with_existing_fixture_jsonl() {
    // Read the first ten rows of each captured history.jsonl and verify
    // they deserialize into Revision and re-serialize byte-identically
    // (after key-order normalization). This is the compatibility
    // guarantee that lets capture-history (Rust) replace
    // scripts/capture_history.py.
    let roots = [
        "../../parity-fixtures/zh/1686258/64806634/history.jsonl",
        "../../parity-fixtures/en/79023819/1277418181/history.jsonl",
        "../../parity-fixtures/simple/27263/10855732/history.jsonl",
        "../../parity-fixtures/en/24544/1354638187/history.jsonl",
    ];
    let mut checked = 0;
    for path in &roots {
        let body = match std::fs::read_to_string(path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        for (i, line) in body.lines().take(10).enumerate() {
            let r: Revision = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("decode {path} line {i}: {e}"));
            // re-encode and compare *keys* only — Python's json.dumps
            // and serde_json use different default whitespace, but
            // the parsed value must be equivalent.
            let parsed_back: serde_json::Value = serde_json::from_str(line).unwrap();
            let re_encoded: serde_json::Value = serde_json::to_value(&r).unwrap();
            assert_eq!(re_encoded, parsed_back,
                "round-trip lossy on {path} line {i}");
            checked += 1;
        }
    }
    assert!(checked >= 8, "expected to round-trip rows from multiple fixtures");
}
