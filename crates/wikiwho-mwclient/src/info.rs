//! `prop=info` response parser.
//!
//! Three [`crate::MwClient`] methods make the same Action API call
//! shape (only the selector query param differs — `titles=`,
//! `pageids=`, or `revids=`), so they share this parser:
//!
//! - [`crate::MwClient::resolve_title`]
//! - [`crate::MwClient::resolve_page_id`]
//! - [`crate::MwClient::resolve_rev_id`]
//!
//! All three return a [`PageInfo`] with `(title, page_id, last_revid)`.
//! For the rev_id-keyed call, `last_revid` is still MW's view of the
//! page's *current* latest — the caller is responsible for overriding
//! it with the request's rev_id if they want the cache-miss fetch to
//! stop at that snapshot rather than the live tip.

use serde::{Deserialize, Serialize};

use crate::{MwError, Result};

/// Minimal page-level metadata returned by the resolve calls. The
/// `title` is the form MW echoes back — i.e. after case normalization
/// and redirect resolution if the API performed either.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageInfo {
    pub title: String,
    pub page_id: u64,
    pub last_revid: u64,
}

/// Parse a `query.pages[0]` envelope into [`PageInfo`].
///
/// Returns `MwError::PageMissing` when MW signals the page is absent
/// or invalid (`missing: true`, `invalid: true`, or — for `revids=`
/// queries — when MW returns `query.badrevids` instead of
/// `query.pages`). Returns `MwError::Shape` when the response doesn't
/// carry the expected `query.pages[0]` block — that usually means MW
/// returned an error envelope at the top level, which is already
/// surfaced by `request_json`.
pub fn parse_page_info(body: &serde_json::Value) -> Result<PageInfo> {
    // `revids=` queries that name a rev_id MW doesn't recognize come
    // back as `{"query": {"badrevids": {...}}}` with no `pages` block.
    // Surface that as `PageMissing` so the caller's handling matches
    // the other "not on MW" paths.
    if body
        .get("query")
        .and_then(|q| q.get("badrevids"))
        .is_some()
    {
        return Err(MwError::PageMissing { page_id: 0 });
    }

    let pages = body
        .get("query")
        .and_then(|q| q.get("pages"))
        .and_then(|p| p.as_array())
        .ok_or_else(|| MwError::Shape("missing query.pages array".into()))?;
    let page = pages
        .first()
        .ok_or_else(|| MwError::Shape("empty query.pages".into()))?;

    if page
        .get("missing")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || page.get("invalid").is_some()
    {
        let page_id = page.get("pageid").and_then(|v| v.as_u64()).unwrap_or(0);
        return Err(MwError::PageMissing { page_id });
    }

    let title = page
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MwError::Shape("page missing title".into()))?
        .to_string();
    let page_id = page
        .get("pageid")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| MwError::Shape("page missing pageid".into()))?;
    let last_revid = page
        .get("lastrevid")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| MwError::Shape("page missing lastrevid".into()))?;

    Ok(PageInfo {
        title,
        page_id,
        last_revid,
    })
}
