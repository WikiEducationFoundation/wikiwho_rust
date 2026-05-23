//! Revision fetcher for the MW Action API `prop=revisions` query.
//!
//! The JSONL output shape ([`Revision`]) is identical to what
//! `scripts/capture_history.py` emits — by design, so the binary
//! `capture-history` can be a drop-in replacement and existing
//! `parity-fixtures/.../history.jsonl` files continue to parse.

use serde::{Deserialize, Serialize};

use crate::{MwClient, MwError, Result};

/// One MediaWiki revision in the shape consumed by
/// `wikiwho-attribute::Article::analyse_revision` (after
/// `text_hidden=true` revisions are skipped, matching `wikiwho.py:144`).
///
/// `parent_id == 0` means the article's first revision; that's how
/// MW signals "no parent" in formatversion=2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Revision {
    pub rev_id: u64,
    #[serde(default)]
    pub parent_id: u64,
    pub timestamp: String,
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub minor: bool,
    #[serde(default)]
    pub user_id: Option<u64>,
    #[serde(default)]
    pub user_name: Option<String>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub text_hidden: bool,
}

/// One page of fetched revisions. `saw_end` is true when this batch
/// contains the requested `end_rev_id` — the fetcher returns at most
/// one more batch (`None`) after that.
#[derive(Debug, Clone)]
pub struct Batch {
    pub revisions: Vec<Revision>,
    pub rvcontinue: Option<String>,
    pub saw_end: bool,
}

/// Async paginator over the MW Action API. Driven by repeated calls
/// to [`RevisionFetcher::next_batch`] until it returns `Ok(None)`.
pub struct RevisionFetcher<'a> {
    client: &'a MwClient,
    page_id: u64,
    end_rev_id: u64,
    rvcontinue: Option<String>,
    finished: bool,
    /// Tracks whether we've already seen the end rev so we can stop
    /// even if MW's `continue` block still claims more.
    saw_end: bool,
    /// Number of batches fetched so far. Used for the inter-batch
    /// polite delay (skipped on the first call).
    batches: u32,
}

impl<'a> RevisionFetcher<'a> {
    pub(crate) fn new(
        client: &'a MwClient,
        page_id: u64,
        end_rev_id: u64,
        rvcontinue: Option<String>,
    ) -> Self {
        Self {
            client,
            page_id,
            end_rev_id,
            rvcontinue,
            finished: false,
            saw_end: false,
            batches: 0,
        }
    }

    pub fn page_id(&self) -> u64 {
        self.page_id
    }

    pub fn end_rev_id(&self) -> u64 {
        self.end_rev_id
    }

    /// The current `rvcontinue` token, or `None` if we're at the
    /// start (or end). Useful for persisting progress between calls.
    pub fn rvcontinue(&self) -> Option<&str> {
        self.rvcontinue.as_deref()
    }

    pub async fn next_batch(&mut self) -> Result<Option<Batch>> {
        if self.finished {
            return Ok(None);
        }

        // Polite delay between batches (skipped on the first call).
        if self.batches > 0 {
            tokio::time::sleep(self.client.between_batches()).await;
        }
        self.batches += 1;

        // formatversion=2; rvslots=main per ../wikiwho_api/api/handler.py:464.
        // The end-of-window param is rvendid, not rvend (timestamp), so we
        // get a precise rev_id cutoff regardless of timestamp ordering.
        let page_id_s = self.page_id.to_string();
        let end_id_s = self.end_rev_id.to_string();
        let mut params: Vec<(&str, &str)> = vec![
            ("action", "query"),
            ("format", "json"),
            ("formatversion", "2"),
            ("prop", "revisions"),
            ("rvprop", "ids|timestamp|user|userid|comment|flags|sha1|content"),
            ("rvlimit", "max"),
            ("rvdir", "newer"),
            ("rvslots", "main"),
            ("rvendid", end_id_s.as_str()),
            ("pageids", page_id_s.as_str()),
        ];
        if let Some(cont) = self.rvcontinue.as_deref() {
            params.push(("rvcontinue", cont));
        }

        let body = self.client.request_json(&params).await?;
        let page = extract_page(&body, self.page_id)?;
        let revisions_raw = page
            .get("revisions")
            .and_then(|v| v.as_array())
            .map(|v| v.as_slice())
            .unwrap_or(&[]);

        let mut revisions = Vec::with_capacity(revisions_raw.len());
        for rev in revisions_raw {
            let r = parse_revision(rev)?;
            if r.rev_id == self.end_rev_id {
                self.saw_end = true;
            }
            revisions.push(r);
        }

        let next_rvcontinue = body
            .get("continue")
            .and_then(|c| c.get("rvcontinue"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Stop when we've seen the target rev OR pagination is done.
        // If pagination ends without seeing the target, that's an
        // error (the target rev_id doesn't belong to this page).
        let done = self.saw_end || next_rvcontinue.is_none();
        if done && !self.saw_end {
            return Err(MwError::PaginationEndedEarly {
                page_id: self.page_id,
                end_rev_id: self.end_rev_id,
            });
        }
        self.rvcontinue = next_rvcontinue.clone();
        if done {
            self.finished = true;
        }

        Ok(Some(Batch {
            revisions,
            rvcontinue: next_rvcontinue,
            saw_end: self.saw_end,
        }))
    }
}

fn extract_page(body: &serde_json::Value, page_id: u64) -> Result<&serde_json::Value> {
    let pages = body
        .get("query")
        .and_then(|q| q.get("pages"))
        .and_then(|p| p.as_array())
        .ok_or_else(|| MwError::Shape("missing query.pages array (formatversion=2)".into()))?;
    let page = pages
        .first()
        .ok_or_else(|| MwError::Shape(format!("empty query.pages for page_id={page_id}")))?;
    if page
        .get("missing")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Err(MwError::PageMissing { page_id });
    }
    Ok(page)
}

/// Convert one MW Action API revision object to our [`Revision`].
///
/// formatversion=2 quirks (mirrors `scripts/capture_history.py` and the
/// fix we landed for the minor-edit flag bug):
///
/// - `minor` is always present as a bool in fv=2; the fv=1 "key absent
///   means false" idiom is wrong here.
/// - `userhidden`, `commenthidden`, `suppressed`, `sha1hidden` use
///   presence-when-true.
/// - `texthidden` lives at `slots.main.texthidden`, never at the top
///   level. `textmissing` and `suppressed` also imply text is unusable.
pub fn parse_revision(rev: &serde_json::Value) -> Result<Revision> {
    let rev_id = rev
        .get("revid")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| MwError::Shape("revision missing revid".into()))?;

    let parent_id = rev.get("parentid").and_then(|v| v.as_u64()).unwrap_or(0);

    let timestamp = rev
        .get("timestamp")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MwError::Shape(format!("revision {rev_id} missing timestamp")))?
        .to_string();

    let slot = rev.get("slots").and_then(|s| s.get("main"));
    let text_hidden = slot
        .and_then(|m| m.get("texthidden"))
        .map(|v| v.is_boolean() && v.as_bool().unwrap_or(false) || v.is_string() || v.is_null())
        .unwrap_or(false)
        || rev.get("textmissing").is_some()
        || rev.get("suppressed").is_some();

    let text = if text_hidden {
        String::new()
    } else {
        slot.and_then(|m| m.get("content").or_else(|| m.get("*")))
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| {
                rev.get("*")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
            })
            .to_string()
    };

    let user_hidden = rev.get("userhidden").is_some();
    let comment_hidden =
        rev.get("commenthidden").is_some() || rev.get("suppressed").is_some();

    let user_id = if user_hidden {
        None
    } else {
        rev.get("userid").and_then(|v| v.as_u64())
    };
    let user_name = if user_hidden {
        None
    } else {
        rev.get("user").and_then(|v| v.as_str()).map(str::to_string)
    };
    let comment = if comment_hidden {
        None
    } else {
        rev.get("comment").and_then(|v| v.as_str()).map(str::to_string)
    };
    let sha1 = rev.get("sha1").and_then(|v| v.as_str()).map(str::to_string);
    let minor = rev.get("minor").and_then(|v| v.as_bool()).unwrap_or(false);

    Ok(Revision {
        rev_id,
        parent_id,
        timestamp,
        sha1,
        comment,
        minor,
        user_id,
        user_name,
        text,
        text_hidden,
    })
}
