//! `list=users&ususerids=...` response parser.
//!
//! Used by [`crate::MwClient::resolve_users`] for WhoColor's editor-
//! name resolution. The response shape is a flat list under
//! `query.users` where each entry has `userid` + (optionally) `name`.
//! Missing or hidden users may omit `name`; we skip those silently.

use crate::{MwError, Result};

/// Parse a `list=users` response body into `(user_id, name)` pairs.
///
/// Returns `MwError::Shape` if the envelope is missing or malformed
/// (no `query.users` array). Unknown user_ids that MW returns with
/// `missing: true` are skipped — the caller will fall back to the
/// raw editor string when no name is found.
pub fn parse_users_response(body: &serde_json::Value) -> Result<Vec<(u64, String)>> {
    let users = body
        .get("query")
        .and_then(|q| q.get("users"))
        .and_then(|u| u.as_array())
        .ok_or_else(|| MwError::Shape("missing query.users array".into()))?;
    let mut out = Vec::with_capacity(users.len());
    for u in users {
        // Skip missing / invalid entries — they have no `userid` we
        // can act on, or no `name` to return.
        let Some(uid) = u.get("userid").and_then(|v| v.as_u64()) else {
            continue;
        };
        if u.get("missing").and_then(|v| v.as_bool()).unwrap_or(false) {
            continue;
        }
        let Some(name) = u.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        out.push((uid, name.to_string()));
    }
    Ok(out)
}
