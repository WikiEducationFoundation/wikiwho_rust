//! MediaWiki Action API + Wikipedia REST client.
//!
//! Used by `wikiwho-server` (cache-miss fetches: ingest revisions up
//! through a target `rev_id` and feed them to `Article::analyse_revision`)
//! and `wikiwho-ingest` (catch-up gaps between dump bootstrap and the
//! live EventStreams feed).
//!
//! Mirrors `../wikiwho_api/api/handler.py` for parameter choice and
//! retry policy:
//!
//! - `rvprop = ids|timestamp|user|userid|comment|flags|sha1|content`
//! - `rvlimit = max` (50 anon, 500 with `apihighlimits`)
//! - `rvdir = newer`, `rvslots = main`, `formatversion = 2`
//! - On HTTP 429, sleep for `Retry-After` (or exponential backoff if
//!   absent) until the cumulative budget is exhausted.
//! - On 5xx, retry with exponential backoff up to `retry_max_attempts`.
//!
//! The crate is async (tokio + reqwest); the binary `capture-history`
//! provides a CLI replacement for `scripts/capture_history.py` that
//! emits the same JSONL shape (one revision per line).

use std::collections::HashMap;
use std::time::Duration;

use reqwest::Client;

pub mod info;
pub mod revisions;
pub mod users;

pub use info::{PageInfo, parse_page_info};
pub use revisions::{Batch, Revision, RevisionFetcher};
pub use users::parse_users_response;

/// Default User-Agent string. Wikimedia policy requires a contact
/// address; tweak via [`MwClient::builder`] if shipping under a
/// different deployment.
pub const DEFAULT_USER_AGENT: &str = concat!(
    "wikiwho_rust-mwclient/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/WikiEducationFoundation; sage@wikiedu.org)"
);

/// Max user_ids per `list=users&ususerids=` request. MW caps anonymous
/// callers at 50; bot accounts can do 500 with `apihighlimits`. The
/// Python reference at `WhoColor/utils.py:103` also uses 50.
pub const MW_USERS_BATCH_SIZE: usize = 50;

/// Errors surfaced by the MW client. `Api` is a structured MW error
/// (the `{"error": {"code", "info"}}` envelope); `RateLimitBudgetExhausted`
/// is a soft cap we control so a misbehaving wiki can't pin us
/// indefinitely.
#[derive(Debug, thiserror::Error)]
pub enum MwError {
    #[error("HTTP transport error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("MW API error: {code}: {info}")]
    Api { code: String, info: String },

    #[error("article missing (page_id={page_id})")]
    PageMissing { page_id: u64 },

    #[error("rate-limit budget exhausted after {slept_seconds}s; last Retry-After was {last_retry_after_seconds}s")]
    RateLimitBudgetExhausted {
        slept_seconds: u64,
        last_retry_after_seconds: u64,
    },

    #[error("pagination ended before reaching rev_id={end_rev_id} for page_id={page_id}")]
    PaginationEndedEarly { page_id: u64, end_rev_id: u64 },

    #[error("exhausted retries: {0}")]
    Retry(String),

    #[error("unexpected response shape: {0}")]
    Shape(String),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, MwError>;

/// Asynchronous client for one MediaWiki host (one language). The
/// client owns a `reqwest::Client` with connection pooling enabled —
/// share an `MwClient` across many calls rather than constructing one
/// per request.
#[derive(Debug, Clone)]
pub struct MwClient {
    api_url: String,
    /// Base URL of the wiki's REST API (e.g.
    /// `https://en.wikipedia.org/api/rest_v1`). Used by
    /// `fetch_parsoid_html` for the `/page/html/{title}/{rev_id}`
    /// endpoint. Derived from the Action API URL by default; tests
    /// can override via [`MwClientBuilder::rest_base_url`].
    rest_base_url: String,
    http: Client,
    user_agent: String,
    between_batches: Duration,
    retry_budget: Duration,
    retry_max_attempts: u32,
    base_backoff: Duration,
}

impl MwClient {
    /// Construct a default client for `{lang}.wikipedia.org`. For test
    /// or custom hosts use [`MwClient::builder`].
    pub fn new(lang: &str) -> Result<Self> {
        Self::builder(lang).build()
    }

    pub fn builder(lang: &str) -> MwClientBuilder {
        MwClientBuilder::for_lang(lang)
    }

    pub fn api_url(&self) -> &str {
        &self.api_url
    }

    pub fn rest_base_url(&self) -> &str {
        &self.rest_base_url
    }

    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    pub fn between_batches(&self) -> Duration {
        self.between_batches
    }

    /// Fetch revisions of `page_id` up to and including `end_rev_id`,
    /// oldest-first. The returned [`RevisionFetcher`] is a paginated
    /// async iterator; call [`RevisionFetcher::next_batch`] in a loop
    /// until it returns `None`.
    pub fn fetch_revisions(&self, page_id: u64, end_rev_id: u64) -> RevisionFetcher<'_> {
        RevisionFetcher::new(self, page_id, end_rev_id, None)
    }

    /// Resolve a title to a [`PageInfo`] via the Action API
    /// `prop=info&inprop=lastrevid`. MW handles case normalization and
    /// redirect resolution server-side; the returned `title` is the
    /// canonical form MW echoes back, so on-disk storage stays
    /// consistent with the API.
    pub async fn resolve_title(&self, title: &str) -> Result<PageInfo> {
        let body = self
            .request_json(&[
                ("action", "query"),
                ("format", "json"),
                ("formatversion", "2"),
                ("prop", "info"),
                ("inprop", "lastrevid"),
                ("titles", title),
            ])
            .await?;
        parse_page_info(&body)
    }

    /// Resolve a `page_id` to a [`PageInfo`] via the Action API. Used
    /// by the page_id endpoints' cache-miss path to learn the latest
    /// rev_id (and the stored title) without an extra round-trip.
    pub async fn resolve_page_id(&self, page_id: u64) -> Result<PageInfo> {
        let page_id_s = page_id.to_string();
        let body = self
            .request_json(&[
                ("action", "query"),
                ("format", "json"),
                ("formatversion", "2"),
                ("prop", "info"),
                ("inprop", "lastrevid"),
                ("pageids", page_id_s.as_str()),
            ])
            .await?;
        parse_page_info(&body)
    }

    /// Resolve a `rev_id` to a [`PageInfo`] via the Action API. Used
    /// by endpoint 1's cache-miss path (`/rev_content/rev_id/{rev_id}/`)
    /// to learn the article the rev_id belongs to without taking a
    /// round-trip through the title or page_id endpoints first.
    ///
    /// The returned `last_revid` is MW's view of the **page's** current
    /// latest, not the queried rev_id. Endpoint 1's handler overrides
    /// it with the request's rev_id so the cache-miss fetch stops at
    /// the requested snapshot rather than the live tip.
    pub async fn resolve_rev_id(&self, rev_id: u64) -> Result<PageInfo> {
        let rev_id_s = rev_id.to_string();
        let body = self
            .request_json(&[
                ("action", "query"),
                ("format", "json"),
                ("formatversion", "2"),
                ("prop", "info"),
                ("inprop", "lastrevid"),
                ("revids", rev_id_s.as_str()),
            ])
            .await?;
        parse_page_info(&body)
    }

    /// Resolve a batch of `user_id`s to `(user_id, user_name)` pairs.
    ///
    /// Used by the WhoColor endpoint to map editor IDs to display
    /// names — see API.md §7. Anonymous editors (those whose `editor`
    /// string starts with `0|`) are excluded by the caller; this
    /// method only handles registered users.
    ///
    /// MW's `list=users&ususerids=` accepts up to 50 IDs per request
    /// for anonymous callers (500 with `apihighlimits`). We batch in
    /// groups of `MW_USERS_BATCH_SIZE` and concatenate. Unknown IDs
    /// are silently omitted from the result map.
    pub async fn resolve_users(&self, user_ids: &[u64]) -> Result<HashMap<u64, String>> {
        let mut names: HashMap<u64, String> = HashMap::with_capacity(user_ids.len());
        if user_ids.is_empty() {
            return Ok(names);
        }
        // Dedupe + stable order so the test surface is deterministic.
        let mut unique: Vec<u64> = user_ids.to_vec();
        unique.sort_unstable();
        unique.dedup();

        for chunk in unique.chunks(MW_USERS_BATCH_SIZE) {
            let ids_param = chunk
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join("|");
            let body = self
                .request_json(&[
                    ("action", "query"),
                    ("format", "json"),
                    ("formatversion", "2"),
                    ("list", "users"),
                    ("ususerids", ids_param.as_str()),
                ])
                .await?;
            for (id, name) in parse_users_response(&body)? {
                names.insert(id, name);
            }
        }
        Ok(names)
    }

    /// Fetch Parsoid HTML for a specific `(title, rev_id)` from the
    /// wiki's REST API (`/api/rest_v1/page/html/{title}/{rev_id}`).
    /// Used by the WhoColor endpoint as the substrate the
    /// token-spans get injected into.
    ///
    /// PLAN.md §4.6 settled on this endpoint as the HTML source. WMF
    /// caches it aggressively at the edge; the response body is
    /// generally immutable per `(lang, rev_id)`.
    pub async fn fetch_parsoid_html(&self, title: &str, rev_id: u64) -> Result<String> {
        let url = format!(
            "{base}/page/html/{title}/{rev_id}",
            base = self.rest_base_url.trim_end_matches('/'),
            title = urlencoding::encode(title),
        );
        self.request_text(&url).await
    }

    /// Low-level GET that returns the response body as a string.
    /// Shared retry/backoff policy with [`Self::request_json`] but
    /// doesn't try to JSON-decode the body. Used for Parsoid HTML.
    async fn request_text(&self, url: &str) -> Result<String> {
        let mut attempt = 0u32;
        let mut slept: Duration = Duration::ZERO;

        loop {
            attempt += 1;
            let response = self
                .http
                .get(url)
                .header("User-Agent", &self.user_agent)
                .send()
                .await;
            let response = match response {
                Ok(r) => r,
                Err(e) if attempt < self.retry_max_attempts && transient(&e) => {
                    tokio::time::sleep(self.backoff_for(attempt)).await;
                    continue;
                }
                Err(e) => return Err(MwError::Http(e)),
            };
            let status = response.status();
            if status.as_u16() == 429 {
                let ra = response
                    .headers()
                    .get("Retry-After")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(Duration::from_secs)
                    .unwrap_or_else(|| self.backoff_for(attempt));
                let remaining = self.retry_budget.saturating_sub(slept);
                if remaining.is_zero() {
                    return Err(MwError::RateLimitBudgetExhausted {
                        slept_seconds: slept.as_secs(),
                        last_retry_after_seconds: ra.as_secs(),
                    });
                }
                let sleep_for = ra.min(remaining);
                slept += sleep_for;
                tokio::time::sleep(sleep_for).await;
                continue;
            }
            if status.as_u16() == 404 {
                return Err(MwError::PageMissing { page_id: 0 });
            }
            if status.is_server_error() && attempt < self.retry_max_attempts {
                tokio::time::sleep(self.backoff_for(attempt)).await;
                continue;
            }
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(MwError::Shape(format!(
                    "HTTP {}: {}",
                    status,
                    truncate(&body, 300)
                )));
            }
            return response.text().await.map_err(MwError::Http);
        }
    }

    /// Resume an in-progress fetch from a saved `rvcontinue` token.
    /// Use when continuing an article whose previous batches have
    /// already been processed and stored.
    pub fn fetch_revisions_from(
        &self,
        page_id: u64,
        end_rev_id: u64,
        rvcontinue: impl Into<String>,
    ) -> RevisionFetcher<'_> {
        RevisionFetcher::new(self, page_id, end_rev_id, Some(rvcontinue.into()))
    }

    /// Low-level request helper with retry. Used by the fetcher;
    /// public so callers building other endpoints can reuse it.
    pub async fn request_json(&self, params: &[(&str, &str)]) -> Result<serde_json::Value> {
        let mut attempt = 0u32;
        let mut slept: Duration = Duration::ZERO;
        let mut last_retry_after: Duration = Duration::ZERO;

        loop {
            attempt += 1;
            let response = self
                .http
                .get(&self.api_url)
                .query(params)
                .header("User-Agent", &self.user_agent)
                .send()
                .await;

            let response = match response {
                Ok(r) => r,
                Err(e) if attempt < self.retry_max_attempts && transient(&e) => {
                    let delay = self.backoff_for(attempt);
                    tokio::time::sleep(delay).await;
                    continue;
                }
                Err(e) => return Err(MwError::Http(e)),
            };

            let status = response.status();

            if status.as_u16() == 429 {
                let ra = response
                    .headers()
                    .get("Retry-After")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(Duration::from_secs)
                    .unwrap_or_else(|| self.backoff_for(attempt));
                last_retry_after = ra;
                let remaining = self.retry_budget.saturating_sub(slept);
                if remaining.is_zero() {
                    return Err(MwError::RateLimitBudgetExhausted {
                        slept_seconds: slept.as_secs(),
                        last_retry_after_seconds: ra.as_secs(),
                    });
                }
                let sleep_for = ra.min(remaining);
                slept += sleep_for;
                tokio::time::sleep(sleep_for).await;
                if sleep_for < ra {
                    return Err(MwError::RateLimitBudgetExhausted {
                        slept_seconds: slept.as_secs(),
                        last_retry_after_seconds: ra.as_secs(),
                    });
                }
                continue;
            }

            if status.is_server_error() {
                if attempt < self.retry_max_attempts {
                    let delay = self.backoff_for(attempt);
                    tokio::time::sleep(delay).await;
                    continue;
                }
                let body = response.text().await.unwrap_or_default();
                return Err(MwError::Retry(format!(
                    "HTTP {} after {} attempts: {}",
                    status,
                    attempt,
                    truncate(&body, 300)
                )));
            }

            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(MwError::Shape(format!(
                    "HTTP {}: {}",
                    status,
                    truncate(&body, 300)
                )));
            }

            let body: serde_json::Value = response.json().await?;
            if let Some(err) = body.get("error") {
                let code = err
                    .get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let info = err
                    .get("info")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                return Err(MwError::Api { code, info });
            }
            let _ = last_retry_after; // silence unused on success path
            return Ok(body);
        }
    }

    fn backoff_for(&self, attempt: u32) -> Duration {
        let factor = 1u32.checked_shl(attempt.saturating_sub(1)).unwrap_or(u32::MAX);
        self.base_backoff.saturating_mul(factor)
    }
}

fn transient(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

/// Builder for [`MwClient`]. Defaults are tuned for polite long-running
/// catch-up jobs (300ms between batches, 5-minute rate-limit budget,
/// 5 retry attempts, 1-second base backoff).
#[derive(Debug, Clone)]
pub struct MwClientBuilder {
    api_url: String,
    rest_base_url: String,
    user_agent: String,
    between_batches: Duration,
    retry_budget: Duration,
    retry_max_attempts: u32,
    base_backoff: Duration,
    request_timeout: Duration,
}

impl MwClientBuilder {
    /// Start from a language code that maps to `{lang}.wikipedia.org`.
    pub fn for_lang(lang: &str) -> Self {
        Self {
            api_url: format!("https://{lang}.wikipedia.org/w/api.php"),
            rest_base_url: format!("https://{lang}.wikipedia.org/api/rest_v1"),
            user_agent: DEFAULT_USER_AGENT.to_string(),
            between_batches: Duration::from_millis(300),
            retry_budget: Duration::from_secs(300),
            retry_max_attempts: 5,
            base_backoff: Duration::from_secs(1),
            request_timeout: Duration::from_secs(180),
        }
    }

    /// Start from a full API URL (e.g. a mock server in tests). The
    /// REST base URL is left empty; set it explicitly via
    /// [`Self::rest_base_url`] if the test needs to exercise Parsoid
    /// HTML fetches.
    pub fn for_api_url(url: impl Into<String>) -> Self {
        Self {
            api_url: url.into(),
            rest_base_url: String::new(),
            user_agent: DEFAULT_USER_AGENT.to_string(),
            between_batches: Duration::from_millis(300),
            retry_budget: Duration::from_secs(300),
            retry_max_attempts: 5,
            base_backoff: Duration::from_secs(1),
            request_timeout: Duration::from_secs(180),
        }
    }

    /// Override the REST API base URL. Tests use this to point Parsoid
    /// HTML fetches at a mock server.
    pub fn rest_base_url(mut self, url: impl Into<String>) -> Self {
        self.rest_base_url = url.into();
        self
    }

    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self
    }

    pub fn between_batches(mut self, d: Duration) -> Self {
        self.between_batches = d;
        self
    }

    pub fn retry_budget(mut self, d: Duration) -> Self {
        self.retry_budget = d;
        self
    }

    pub fn retry_max_attempts(mut self, n: u32) -> Self {
        self.retry_max_attempts = n.max(1);
        self
    }

    pub fn base_backoff(mut self, d: Duration) -> Self {
        self.base_backoff = d;
        self
    }

    pub fn request_timeout(mut self, d: Duration) -> Self {
        self.request_timeout = d;
        self
    }

    pub fn build(self) -> Result<MwClient> {
        let http = Client::builder()
            .timeout(self.request_timeout)
            .pool_max_idle_per_host(4)
            .build()?;
        Ok(MwClient {
            api_url: self.api_url,
            rest_base_url: self.rest_base_url,
            http,
            user_agent: self.user_agent,
            between_batches: self.between_batches,
            retry_budget: self.retry_budget,
            retry_max_attempts: self.retry_max_attempts,
            base_backoff: self.base_backoff,
        })
    }
}
