//! Wire-format response builders (API.md §1–6).
//!
//! Converts an in-memory [`Article`] into the JSON the HTTP API serves.
//! Lives in this crate rather than `wikiwho-server` because the shape
//! is pure algorithm state → JSON; there's no HTTP machinery here. The
//! server crate will wrap these into axum handlers later.
//!
//! Mirrors `../wikiwho_api/wikiwho/wikiwho_simple.py:23-71` (`get_revision_content`).
//!
//! Field order matches the Python reference so a byte-level comparison
//! against captured `rev_content.json` fixtures works (serde_json
//! preserves struct field declaration order on serialization).

use serde::Serialize;
use serde_json::{Map, Value};

use crate::structures::{Article, RevId, iter_rev_tokens};

/// Which token fields to include in the response. All endpoints in API.md
/// §1-6 accept these as opt-in `=true` query parameters; `str` is
/// always included.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ResponseParameters {
    pub o_rev_id: bool,
    pub editor: bool,
    pub token_id: bool,
    pub inbound: bool,
    pub outbound: bool,
}

impl ResponseParameters {
    /// All fields on. The shape captured `rev_content.json` fixtures
    /// use; parity tests need this.
    pub const ALL: Self = Self {
        o_rev_id: true,
        editor: true,
        token_id: true,
        inbound: true,
        outbound: true,
    };

    /// All fields off (only `str`).
    pub const NONE: Self = Self {
        o_rev_id: false,
        editor: false,
        token_id: false,
        inbound: false,
        outbound: false,
    };
}

/// The top-level envelope of a successful rev_content response
/// (`API.md` §1). `revisions` is a list — for a single-rev request it
/// has one entry, for a range it has the slice.
///
/// `revisions[i]` is a single-key object: the rev id stringified maps
/// to `{editor, time, tokens}`. We model it as a [`serde_json::Value`]
/// so we can preserve the single-key-object curiosity faithfully.
#[derive(Debug, Serialize)]
pub struct RevContentResponse {
    pub article_title: String,
    pub page_id: u64,
    pub success: bool,
    pub message: Option<String>,
    pub revisions: Vec<Value>,
}

/// Error envelope (API.md §1 "Response (200, error)") — used when the
/// requested rev_id isn't in the article history or is spam-flagged.
#[derive(Debug, Serialize)]
pub struct RevContentError {
    #[serde(rename = "Error")]
    pub error: String,
}

/// Build the rev_content response for `revision_ids`.
///
/// Following `wikiwho_simple.py:43-47`, a slice of *two* rev ids is
/// treated as a half-open range `[start, end)` — meaning the entries
/// at indices `[ordered_revisions.index(start) .. ordered_revisions.index(end)]`
/// of `Article::ordered_revisions`. A slice of one is just that single
/// rev. Any other length is an error.
///
/// Returns `Err(RevContentError)` if any requested rev is missing
/// (deleted / spam / never existed); the caller should serve it with
/// HTTP 400 per API.md.
pub fn build_rev_content(
    article: &Article,
    revision_ids: &[RevId],
    params: ResponseParameters,
) -> Result<RevContentResponse, RevContentError> {
    if revision_ids.is_empty() || revision_ids.len() > 2 {
        return Err(RevContentError {
            error: format!(
                "Expected 1 or 2 revision ids, got {}",
                revision_ids.len()
            ),
        });
    }

    for rev_id in revision_ids {
        if !article.revisions.contains_key(rev_id) {
            return Err(RevContentError {
                error: format!(
                    "Revision ID ({rev_id}) does not exist or is spam or deleted!"
                ),
            });
        }
    }

    let effective: Vec<RevId> = if revision_ids.len() == 2 {
        let start = revision_ids[0];
        let end = revision_ids[1];
        let start_idx = article
            .ordered_revisions
            .iter()
            .position(|r| *r == start)
            .ok_or_else(|| RevContentError {
                error: format!("Revision ID ({start}) does not exist or is spam or deleted!"),
            })?;
        let end_idx = article
            .ordered_revisions
            .iter()
            .position(|r| *r == end)
            .ok_or_else(|| RevContentError {
                error: format!("Revision ID ({end}) does not exist or is spam or deleted!"),
            })?;
        article.ordered_revisions[start_idx..end_idx].to_vec()
    } else {
        vec![revision_ids[0]]
    };

    let revisions_json: Vec<Value> = effective
        .iter()
        .map(|rev_id| build_revision_entry(article, *rev_id, params))
        .collect();

    Ok(RevContentResponse {
        article_title: article.title.clone(),
        page_id: article.page_id.unwrap_or(0),
        success: true,
        message: None,
        revisions: revisions_json,
    })
}

/// Build one element of the `revisions` list: a single-key object
/// `{<rev_id_str>: {editor, time, tokens}}`. Token field order matches
/// the Python reference (`wikiwho_simple.py:58-69`): str, o_rev_id,
/// editor, token_id, in, out — fields off in `params` are simply
/// omitted from the per-token object.
fn build_revision_entry(article: &Article, rev_id: RevId, params: ResponseParameters) -> Value {
    let revision = &article.revisions[&rev_id];
    let token_ids = iter_rev_tokens(article, revision);

    let tokens: Vec<Value> = token_ids
        .iter()
        .map(|&tid| build_token_entry(article, tid, params))
        .collect();

    let mut inner = Map::new();
    inner.insert("editor".into(), Value::String(revision.editor.clone()));
    inner.insert("time".into(), Value::String(revision.timestamp.clone()));
    inner.insert("tokens".into(), Value::Array(tokens));

    let mut outer = Map::new();
    outer.insert(rev_id.to_string(), Value::Object(inner));
    Value::Object(outer)
}

fn build_token_entry(
    article: &crate::structures::Article,
    token_id: crate::structures::TokenId,
    params: ResponseParameters,
) -> Value {
    let w = article.word(token_id);
    let mut map = Map::new();
    map.insert("str".into(), Value::String(w.value.clone()));
    if params.o_rev_id {
        map.insert("o_rev_id".into(), Value::Number(w.origin_rev_id.into()));
    }
    if params.editor {
        let editor = article
            .revisions
            .get(&w.origin_rev_id)
            .map(|r| r.editor.clone())
            .unwrap_or_default();
        map.insert("editor".into(), Value::String(editor));
    }
    if params.token_id {
        map.insert("token_id".into(), Value::Number(w.token_id.into()));
    }
    if params.inbound {
        let arr: Vec<Value> = w.inbound.iter().map(|r| Value::Number((*r).into())).collect();
        map.insert("in".into(), Value::Array(arr));
    }
    if params.outbound {
        let arr: Vec<Value> = w.outbound.iter().map(|r| Value::Number((*r).into())).collect();
        map.insert("out".into(), Value::Array(arr));
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::{RevisionInput, RevisionOutcome};
    use crate::structures::Article;

    fn fixture_article() -> Article {
        let mut article = Article::new("Demo");
        article.page_id = Some(7);
        let mut feed = |rev_id: u64, user_id: u64, ts: &str, text: &str| {
            article.analyse_revision(RevisionInput {
                rev_id,
                timestamp: ts.into(),
                user_id: Some(user_id),
                user_name: Some(format!("u{user_id}")),
                comment: None,
                minor: false,
                sha1: None,
                text: text.into(),
            })
        };
        assert_eq!(
            feed(101, 11, "2024-01-01T00:00:00Z", "Hello there friend."),
            RevisionOutcome::Stored
        );
        assert_eq!(
            feed(102, 22, "2024-01-02T00:00:00Z", "Hello dear friend."),
            RevisionOutcome::Stored
        );
        article
    }

    #[test]
    fn envelope_matches_python_reference_shape() {
        let article = fixture_article();
        let resp = build_rev_content(&article, &[102], ResponseParameters::ALL).unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["article_title"], "Demo");
        assert_eq!(v["page_id"], 7);
        assert_eq!(v["success"], true);
        assert_eq!(v["message"], Value::Null);
        assert_eq!(v["revisions"].as_array().unwrap().len(), 1);
        // Single-key object inside the revisions list.
        let inner = &v["revisions"][0];
        let key = inner.as_object().unwrap().keys().next().unwrap();
        assert_eq!(key, "102");
        let body = &inner[key];
        assert!(body["editor"].is_string());
        assert!(body["time"].is_string());
        assert!(body["tokens"].is_array());
    }

    #[test]
    fn token_omits_unrequested_fields() {
        let article = fixture_article();
        let resp = build_rev_content(&article, &[102], ResponseParameters::NONE).unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        let first_token = &v["revisions"][0]["102"]["tokens"][0];
        assert!(first_token.get("str").is_some());
        assert!(first_token.get("o_rev_id").is_none());
        assert!(first_token.get("editor").is_none());
        assert!(first_token.get("token_id").is_none());
        assert!(first_token.get("in").is_none());
        assert!(first_token.get("out").is_none());
    }

    #[test]
    fn token_field_order_matches_python() {
        let article = fixture_article();
        let resp = build_rev_content(&article, &[102], ResponseParameters::ALL).unwrap();
        let s = serde_json::to_string(&resp).unwrap();
        // Find a token's keys in serialized order. The Python reference
        // emits them in the order str, o_rev_id, editor, token_id, in, out.
        let token_start = s
            .find(r#""tokens":[{"#)
            .expect("tokens array present");
        let sub = &s[token_start + r#""tokens":[{"#.len()..];
        let cut = sub.find('}').expect("token object closer");
        let token_blob = &sub[..cut];
        let keys_in_order: Vec<&str> = token_blob
            .split(',')
            .filter_map(|pair| {
                let p = pair.trim_start_matches(',');
                let k_start = p.find('"')?;
                let after = &p[k_start + 1..];
                let k_end = after.find('"')?;
                Some(&after[..k_end])
            })
            .collect();
        assert_eq!(
            keys_in_order,
            vec!["str", "o_rev_id", "editor", "token_id", "in", "out"],
            "actual blob: {token_blob}"
        );
    }

    #[test]
    fn missing_rev_id_returns_python_error_shape() {
        let article = fixture_article();
        let err = build_rev_content(&article, &[9999], ResponseParameters::ALL).unwrap_err();
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["Error"], "Revision ID (9999) does not exist or is spam or deleted!");
        assert!(v.get("error").is_none(), "key must be capital 'Error', not 'error'");
    }

    #[test]
    fn two_rev_ids_is_half_open_range() {
        // wikiwho_simple.py:46 — ordered_revisions[start_index:end_index]
        // is half-open (Python list slicing): start INCLUSIVE, end EXCLUSIVE.
        let mut article = fixture_article();
        article.analyse_revision(RevisionInput {
            rev_id: 103,
            timestamp: "2024-01-03T00:00:00Z".into(),
            user_id: Some(33),
            user_name: Some("u33".into()),
            comment: None,
            minor: false,
            sha1: None,
            text: "Hello dear best friend.".into(),
        });
        // ordered_revisions is [101, 102, 103]. Range [101, 103) →
        // entries at indices 0..2 → rev_ids [101, 102].
        let resp = build_rev_content(&article, &[101, 103], ResponseParameters::NONE).unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        let ids: Vec<&str> = v["revisions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_object().unwrap().keys().next().unwrap().as_str())
            .collect();
        assert_eq!(ids, vec!["101", "102"]);
    }
}
