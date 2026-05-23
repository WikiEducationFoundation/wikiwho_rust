//! `prop=info&inprop=lastrevid` response parser.
//!
//! Both [`crate::MwClient::resolve_title`] and
//! [`crate::MwClient::resolve_page_id`] make the same Action API call
//! shape (only the `titles=`/`pageids=` query param differs), so they
//! share this parser.

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
/// or invalid (`missing: true` or `invalid: true`). Returns
/// `MwError::Shape` when the response doesn't carry the expected
/// `query.pages[0]` block — that usually means MW returned an error
/// envelope at the top level, which is already surfaced by
/// `request_json`.
pub fn parse_page_info(body: &serde_json::Value) -> Result<PageInfo> {
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
