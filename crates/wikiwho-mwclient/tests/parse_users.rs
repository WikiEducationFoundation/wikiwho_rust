//! Unit tests for `wikiwho_mwclient::parse_users_response`.
//!
//! No HTTP — exercises the body parser against hand-crafted Action
//! API responses captured from `list=users&ususerids=...` queries.

use serde_json::json;
use wikiwho_mwclient::{MwError, parse_users_response};

#[test]
fn typical_users_parses_id_name_pairs() {
    let body = json!({
        "batchcomplete": true,
        "query": {
            "users": [
                {"userid": 1465, "name": "Froderik"},
                {"userid": 5959, "name": "Valery Beaud"}
            ]
        }
    });
    let out = parse_users_response(&body).unwrap();
    assert_eq!(out, vec![(1465, "Froderik".into()), (5959, "Valery Beaud".into())]);
}

#[test]
fn missing_users_are_silently_skipped() {
    let body = json!({
        "query": {
            "users": [
                {"userid": 1465, "name": "Froderik"},
                {"userid": 9_999_999_999u64, "missing": true},
                {"userid": 5959, "name": "Valery Beaud"}
            ]
        }
    });
    let out = parse_users_response(&body).unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].0, 1465);
    assert_eq!(out[1].0, 5959);
}

#[test]
fn users_without_name_are_skipped() {
    // Hidden / suppressed users may have userid but no name.
    let body = json!({
        "query": {
            "users": [
                {"userid": 1, "name": "A"},
                {"userid": 2},
            ]
        }
    });
    let out = parse_users_response(&body).unwrap();
    assert_eq!(out, vec![(1, "A".into())]);
}

#[test]
fn body_without_query_users_errors_as_shape() {
    let body = json!({"batchcomplete": true});
    let err = parse_users_response(&body).unwrap_err();
    assert!(matches!(err, MwError::Shape(_)));
}

#[test]
fn empty_users_returns_empty_vec() {
    let body = json!({"query": {"users": []}});
    let out = parse_users_response(&body).unwrap();
    assert!(out.is_empty());
}
