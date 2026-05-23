//! Unit tests for `wikiwho_mwclient::parse_page_info`.
//!
//! Doesn't make HTTP calls — exercises the body-parser against
//! hand-crafted Action API responses captured from
//! `prop=info&inprop=lastrevid` queries.

use serde_json::json;
use wikiwho_mwclient::{MwError, parse_page_info};

#[test]
fn typical_page_parses_to_page_info() {
    let body = json!({
        "batchcomplete": true,
        "query": {
            "pages": [{
                "pageid": 24544,
                "ns": 0,
                "title": "Photosynthesis",
                "contentmodel": "wikitext",
                "pagelanguage": "en",
                "touched": "2024-01-01T00:00:00Z",
                "lastrevid": 1099999999u64,
                "length": 100000
            }]
        }
    });
    let info = parse_page_info(&body).unwrap();
    assert_eq!(info.page_id, 24544);
    assert_eq!(info.title, "Photosynthesis");
    assert_eq!(info.last_revid, 1099999999);
}

#[test]
fn normalized_title_echoes_canonical_form() {
    // MW echoes the canonical title in the page record, even when the
    // request used a lowercase form.
    let body = json!({
        "query": {
            "normalized": [{"from": "photosynthesis", "to": "Photosynthesis"}],
            "pages": [{
                "pageid": 24544,
                "title": "Photosynthesis",
                "lastrevid": 1099999999u64
            }]
        }
    });
    let info = parse_page_info(&body).unwrap();
    assert_eq!(info.title, "Photosynthesis");
}

#[test]
fn missing_page_errors_with_page_missing() {
    let body = json!({
        "query": {
            "pages": [{
                "ns": 0,
                "title": "Nonexistent_Article_Title_XYZ",
                "missing": true
            }]
        }
    });
    let err = parse_page_info(&body).unwrap_err();
    assert!(matches!(err, MwError::PageMissing { .. }));
}

#[test]
fn invalid_title_errors_with_page_missing() {
    let body = json!({
        "query": {
            "pages": [{
                "title": "?",
                "invalidreason": "The requested page title is empty or contains only a namespace prefix.",
                "invalid": true
            }]
        }
    });
    let err = parse_page_info(&body).unwrap_err();
    assert!(matches!(err, MwError::PageMissing { .. }));
}

#[test]
fn missing_page_with_pageid_in_response_surfaces_it() {
    let body = json!({
        "query": {
            "pages": [{
                "pageid": 9_999_999_999u64,
                "missing": true
            }]
        }
    });
    match parse_page_info(&body).unwrap_err() {
        MwError::PageMissing { page_id } => assert_eq!(page_id, 9_999_999_999),
        other => panic!("expected PageMissing, got {other:?}"),
    }
}

#[test]
fn revids_query_with_bad_revid_returns_page_missing() {
    // `revids=` queries against MW use `query.badrevids` instead of
    // `query.pages` when the rev_id doesn't exist. We surface that as
    // PageMissing so endpoint 1's cache-miss path doesn't try to
    // process it.
    let body = json!({
        "batchcomplete": true,
        "query": {
            "badrevids": {
                "9999999999": {
                    "revid": 9_999_999_999u64,
                    "missing": true
                }
            }
        }
    });
    match parse_page_info(&body).unwrap_err() {
        // `page_id` field is 0 because we don't know the page; the
        // useful identifier in this case is the rev_id, which the
        // handler has from the request itself.
        MwError::PageMissing { page_id } => assert_eq!(page_id, 0),
        other => panic!("expected PageMissing, got {other:?}"),
    }
}

#[test]
fn revids_query_with_known_revid_parses_like_titles() {
    // The shape MW returns for `prop=info&revids=...` is identical to
    // the title/page_id queries — same `query.pages[0]` with pageid +
    // title + lastrevid. `lastrevid` is the *page's* latest, not the
    // queried rev_id.
    let body = json!({
        "batchcomplete": true,
        "query": {
            "pages": [{
                "pageid": 27263,
                "ns": 0,
                "title": "Wikipedia",
                "lastrevid": 10_855_732u64,
                "length": 17130
            }]
        }
    });
    let info = parse_page_info(&body).unwrap();
    assert_eq!(info.page_id, 27263);
    assert_eq!(info.title, "Wikipedia");
    assert_eq!(info.last_revid, 10_855_732);
}

#[test]
fn body_without_query_errors_as_shape() {
    let body = json!({"batchcomplete": true});
    let err = parse_page_info(&body).unwrap_err();
    assert!(matches!(err, MwError::Shape(_)));
}

#[test]
fn body_with_empty_pages_errors_as_shape() {
    let body = json!({"query": {"pages": []}});
    let err = parse_page_info(&body).unwrap_err();
    assert!(matches!(err, MwError::Shape(_)));
}

#[test]
fn page_missing_lastrevid_errors_as_shape() {
    let body = json!({
        "query": {
            "pages": [{"pageid": 1, "title": "X"}]
        }
    });
    let err = parse_page_info(&body).unwrap_err();
    assert!(matches!(err, MwError::Shape(_)));
}

#[test]
fn page_missing_title_errors_as_shape() {
    let body = json!({
        "query": {
            "pages": [{"pageid": 1, "lastrevid": 2}]
        }
    });
    let err = parse_page_info(&body).unwrap_err();
    assert!(matches!(err, MwError::Shape(_)));
}

#[test]
fn page_missing_pageid_errors_as_shape() {
    let body = json!({
        "query": {
            "pages": [{"title": "X", "lastrevid": 2}]
        }
    });
    let err = parse_page_info(&body).unwrap_err();
    assert!(matches!(err, MwError::Shape(_)));
}
