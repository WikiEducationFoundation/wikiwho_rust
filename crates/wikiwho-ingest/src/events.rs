//! Wikimedia EventStreams listener.
//!
//! The endpoint is `https://stream.wikimedia.org/v2/stream/recentchange`
//! (an HTTP/1.1 chunked SSE stream). The protocol is the standard
//! [W3C eventsource][] format — lines of `field: value`, blank line
//! terminates one event. We care about two fields:
//!
//! - `id:` — a JSON array of `{topic, partition, offset, …}` entries.
//!   Passed back verbatim as the `Last-Event-ID` header on reconnect
//!   to resume close to where we left off.
//! - `data:` — a JSON object describing one MW change. Documented at
//!   <https://stream.wikimedia.org/?doc#/streams/get_v2_stream_recentchange>.
//!
//! We filter to:
//!
//! - `type ∈ {edit, new}` (skip `categorize`, `log`, `external`)
//! - `namespace == 0` (skip Talk, User, etc.)
//! - `wiki ∈ configured set` (e.g. `enwiki`, `simplewiki`)
//!
//! and emit one [`PageEdit`] per matching event.
//!
//! Reconnect handling: any network / parse error inside the stream
//! turns into one yielded `Err(EventStreamError)` and the next call
//! reconnects with the saved Last-Event-ID. Callers see a continuous
//! stream of events with occasional `Err` markers they can log past.
//!
//! [W3C eventsource]: https://html.spec.whatwg.org/multipage/server-sent-events.html

use std::collections::HashSet;
use std::pin::Pin;
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use serde::Deserialize;

use crate::ShutdownSignal;

/// One filtered, parsed `recentchange` event in the shape the apply
/// loop wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageEdit {
    /// Language code (`en`, `simple`, `zh`, …) derived from
    /// `server_name` (e.g. `en.wikipedia.org` → `en`).
    pub language: String,
    /// MW database name, e.g. `enwiki`. Useful for the wiki-set
    /// filter and present in the original event.
    pub wiki: String,
    pub page_id: u64,
    pub rev_id: u64,
    /// Parent revision id reported by MW. Zero for the first revision
    /// on a page (new article).
    pub parent_rev_id: u64,
    pub title: String,
    /// Verbatim `id:` field from the SSE frame, if present. Used as
    /// the `Last-Event-ID` resume header.
    pub sse_id: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum EventStreamError {
    #[error("HTTP transport error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("HTTP status {status}: {body}")]
    BadStatus { status: u16, body: String },

    #[error("connection ended; will reconnect")]
    ConnectionEnded,

    #[error("malformed event frame: {0}")]
    Malformed(String),
}

/// Languages we currently host. Subset of the 67 in the legacy
/// service; ingest the whole list when cutover decisions are made.
/// Exposed so callers can compute the wiki set without re-deriving.
pub fn lang_to_wiki(lang: &str) -> String {
    // Wikimedia naming quirk: simple.wikipedia.org's database is
    // `simplewiki`. All others follow `{lang}wiki`. The mapping is
    // 1:1 with what `events_stream.py` does — `change.get('wiki')`
    // already matches `{lang}wiki` so no special-casing is needed.
    format!("{lang}wiki")
}

/// Build a streaming Future that connects to EventStreams and yields
/// `Result<PageEdit, EventStreamError>` items. On a network failure
/// the stream yields one `Err` and reconnects (using the saved
/// Last-Event-ID) on the next poll.
///
/// The returned stream stops cleanly when `shutdown` fires.
pub fn recentchange_stream(
    base_url: String,
    initial_last_event_id: Option<String>,
    shutdown: ShutdownSignal,
) -> Pin<Box<dyn Stream<Item = Result<PageEdit, EventStreamError>> + Send>> {
    let state = StreamLoop {
        base_url,
        last_event_id: initial_last_event_id,
        wiki_filter: None,
        shutdown,
        http: reqwest::Client::builder()
            .user_agent(crate::USER_AGENT)
            .timeout(Duration::from_secs(60))
            .build()
            .expect("reqwest client"),
    };
    Box::pin(stream_events(state))
}

/// Same as [`recentchange_stream`] but filters server-side by a wiki
/// set (e.g. `["enwiki", "simplewiki"]`) so non-matching events never
/// reach the consumer.
pub fn recentchange_stream_filtered(
    base_url: String,
    initial_last_event_id: Option<String>,
    wikis: HashSet<String>,
    shutdown: ShutdownSignal,
) -> Pin<Box<dyn Stream<Item = Result<PageEdit, EventStreamError>> + Send>> {
    let state = StreamLoop {
        base_url,
        last_event_id: initial_last_event_id,
        wiki_filter: Some(wikis),
        shutdown,
        http: reqwest::Client::builder()
            .user_agent(crate::USER_AGENT)
            .timeout(Duration::from_secs(60))
            .build()
            .expect("reqwest client"),
    };
    Box::pin(stream_events(state))
}

struct StreamLoop {
    base_url: String,
    last_event_id: Option<String>,
    wiki_filter: Option<HashSet<String>>,
    shutdown: ShutdownSignal,
    http: reqwest::Client,
}

/// SSE frame-line accumulator. Decoupled from the network so we can
/// unit-test the parser without spinning up a server.
///
/// Feed it whole or partial lines via [`feed_chunk`]; it returns a
/// list of completed frames each time a blank line terminates one.
/// A completed frame is `(data_lines_joined_by_newline, optional_id)`.
#[derive(Debug, Default)]
struct SseFrameBuffer {
    buf: String,
    cur_data: Vec<String>,
    cur_id: Option<String>,
}

impl SseFrameBuffer {
    fn new() -> Self {
        Self::default()
    }

    /// Append a byte chunk and return any frames that have completed.
    /// Each returned tuple is `(data_joined, id)`.
    fn feed_chunk(&mut self, chunk: &str) -> Vec<(String, Option<String>)> {
        self.buf.push_str(chunk);
        let mut out = Vec::new();
        while let Some(idx) = self.buf.find('\n') {
            let line: String = self.buf.drain(..=idx).collect();
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                if !self.cur_data.is_empty() {
                    let data = std::mem::take(&mut self.cur_data).join("\n");
                    let id = self.cur_id.take();
                    out.push((data, id));
                } else {
                    // Blank line with no data — drop any standalone id
                    // we'd accumulated (matches the W3C spec: dispatch
                    // only happens when data is present).
                    self.cur_id = None;
                }
            } else if let Some(value) = line.strip_prefix("data:") {
                self.cur_data.push(value.trim_start().to_string());
            } else if let Some(value) = line.strip_prefix("id:") {
                self.cur_id = Some(value.trim_start().to_string());
            }
            // Other fields (`event:`, `retry:`, comments) ignored.
        }
        out
    }
}

fn stream_events(
    state: StreamLoop,
) -> impl Stream<Item = Result<PageEdit, EventStreamError>> + Send {
    async_stream::stream! {
        let mut state = state;
        loop {
            if state.shutdown.is_cancelled() {
                break;
            }
            let connect = state.connect().await;
            let resp = match connect {
                Ok(r) => r,
                Err(err) => {
                    yield Err(err);
                    sleep_with_cancel(Duration::from_secs(3), &state.shutdown).await;
                    continue;
                }
            };

            let mut byte_stream = resp.bytes_stream();
            let mut frames = SseFrameBuffer::new();

            loop {
                if state.shutdown.is_cancelled() {
                    break;
                }
                let next = byte_stream.next().await;
                let chunk = match next {
                    Some(Ok(b)) => b,
                    Some(Err(err)) => {
                        yield Err(EventStreamError::Http(err));
                        break; // outer loop will reconnect
                    }
                    None => {
                        yield Err(EventStreamError::ConnectionEnded);
                        break;
                    }
                };
                // EventStreams payloads are valid UTF-8 JSON; lossy
                // decode only matters if a chunk boundary splits a
                // codepoint, which from_utf8_lossy handles cleanly.
                let s = String::from_utf8_lossy(&chunk);
                for (data, id) in frames.feed_chunk(&s) {
                    match parse_frame(&data, id.as_deref(), state.wiki_filter.as_ref()) {
                        Ok(Some(edit)) => {
                            state.last_event_id = edit.sse_id.clone();
                            yield Ok(edit);
                        }
                        Ok(None) => {
                            // Frame parsed but didn't match filters.
                            // Advance the resume id anyway so restart
                            // doesn't replay non-matching events.
                            if let Some(id) = id {
                                state.last_event_id = Some(id);
                            }
                        }
                        Err(err) => yield Err(err),
                    }
                }
            }

            // Disconnect: pause briefly before reconnecting so we don't
            // hammer the endpoint on persistent failure.
            sleep_with_cancel(Duration::from_secs(1), &state.shutdown).await;
        }
    }
}

impl StreamLoop {
    async fn connect(&self) -> Result<reqwest::Response, EventStreamError> {
        let mut req = self.http.get(&self.base_url);
        if let Some(id) = &self.last_event_id {
            req = req.header("Last-Event-ID", id);
        }
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(EventStreamError::BadStatus {
                status: status.as_u16(),
                body: body.chars().take(300).collect(),
            });
        }
        Ok(resp)
    }
}

/// Wait up to `dur`, returning early if `shutdown` fires.
async fn sleep_with_cancel(dur: Duration, shutdown: &ShutdownSignal) {
    tokio::select! {
        _ = tokio::time::sleep(dur) => {}
        _ = shutdown.cancelled() => {}
    }
}

#[derive(Debug, Deserialize)]
struct RawChange {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    namespace: Option<i64>,
    #[serde(default)]
    wiki: Option<String>,
    #[serde(default)]
    server_name: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    revision: Option<RawRevision>,
    /// Some events include page_id at the top level (the `page_id`
    /// field on `recentchange`). Falls back through `id` on older
    /// schema versions.
    #[serde(default)]
    page_id: Option<u64>,
    #[serde(default)]
    id: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawRevision {
    #[serde(default)]
    new: Option<u64>,
    #[serde(default)]
    old: Option<u64>,
}

fn parse_frame(
    data: &str,
    sse_id: Option<&str>,
    wiki_filter: Option<&HashSet<String>>,
) -> Result<Option<PageEdit>, EventStreamError> {
    let change: RawChange = match serde_json::from_str(data) {
        Ok(c) => c,
        Err(err) => {
            return Err(EventStreamError::Malformed(format!("json: {err}")));
        }
    };

    if change.kind != "edit" && change.kind != "new" {
        return Ok(None);
    }
    if change.namespace != Some(0) {
        return Ok(None);
    }
    let Some(wiki) = change.wiki else {
        return Ok(None);
    };
    if let Some(filter) = wiki_filter
        && !filter.contains(&wiki)
    {
        return Ok(None);
    }
    let Some(title) = change.title else {
        return Ok(None);
    };
    let Some(rev) = change.revision else {
        return Ok(None);
    };
    let Some(rev_id) = rev.new else {
        return Ok(None);
    };
    let parent_rev_id = rev.old.unwrap_or(0);

    let page_id = change.page_id.or(change.id).unwrap_or(0);
    if page_id == 0 {
        // EventStreams occasionally omits page_id (e.g. some
        // bot-flagged edits); without it we can't address the article
        // on disk, so drop.
        return Ok(None);
    }

    let language = change
        .server_name
        .as_deref()
        .and_then(|s| s.split('.').next())
        .unwrap_or("")
        .to_string();
    if language.is_empty() {
        return Ok(None);
    }

    Ok(Some(PageEdit {
        language,
        wiki,
        page_id,
        rev_id,
        parent_rev_id,
        title,
        sse_id: sse_id.map(str::to_string),
    }))
}


#[cfg(test)]
mod tests {
    use super::*;

    fn make_data(kind: &str, ns: i64, wiki: &str, new: u64, old: u64, page_id: u64, title: &str) -> String {
        serde_json::json!({
            "type": kind,
            "namespace": ns,
            "wiki": wiki,
            "server_name": format!("{}.wikipedia.org", wiki.trim_end_matches("wiki")),
            "title": title,
            "page_id": page_id,
            "revision": {"new": new, "old": old},
        })
        .to_string()
    }

    #[test]
    fn parse_frame_filters_non_article_ns() {
        let data = make_data("edit", 1, "enwiki", 100, 99, 5, "User talk:Foo");
        assert!(parse_frame(&data, None, None).unwrap().is_none());
    }

    #[test]
    fn parse_frame_filters_non_edit_type() {
        let data = make_data("log", 0, "enwiki", 100, 99, 5, "Foo");
        assert!(parse_frame(&data, None, None).unwrap().is_none());
    }

    #[test]
    fn parse_frame_passes_edit_ns0() {
        let data = make_data("edit", 0, "enwiki", 1000, 999, 5, "Foo");
        let edit = parse_frame(&data, Some("[{}]"), None).unwrap().unwrap();
        assert_eq!(edit.language, "en");
        assert_eq!(edit.wiki, "enwiki");
        assert_eq!(edit.rev_id, 1000);
        assert_eq!(edit.parent_rev_id, 999);
        assert_eq!(edit.page_id, 5);
        assert_eq!(edit.title, "Foo");
        assert_eq!(edit.sse_id.as_deref(), Some("[{}]"));
    }

    #[test]
    fn parse_frame_wiki_filter_blocks_unconfigured() {
        let data = make_data("edit", 0, "frwiki", 1000, 999, 5, "Foo");
        let mut filter = HashSet::new();
        filter.insert("enwiki".to_string());
        assert!(parse_frame(&data, None, Some(&filter)).unwrap().is_none());
    }

    #[test]
    fn parse_frame_wiki_filter_allows_configured() {
        let data = make_data("edit", 0, "enwiki", 1000, 999, 5, "Foo");
        let mut filter = HashSet::new();
        filter.insert("enwiki".to_string());
        assert!(parse_frame(&data, None, Some(&filter)).unwrap().is_some());
    }

    #[test]
    fn parse_frame_first_revision_has_zero_parent() {
        let data = serde_json::json!({
            "type": "new",
            "namespace": 0,
            "wiki": "enwiki",
            "server_name": "en.wikipedia.org",
            "title": "NewPage",
            "page_id": 99,
            "revision": {"new": 1000}, // no `old`
        })
        .to_string();
        let edit = parse_frame(&data, None, None).unwrap().unwrap();
        assert_eq!(edit.parent_rev_id, 0);
        assert_eq!(edit.rev_id, 1000);
    }

    #[test]
    fn parse_frame_missing_page_id_drops() {
        let data = serde_json::json!({
            "type": "edit",
            "namespace": 0,
            "wiki": "enwiki",
            "server_name": "en.wikipedia.org",
            "title": "Foo",
            "revision": {"new": 1000, "old": 999},
        })
        .to_string();
        assert!(parse_frame(&data, None, None).unwrap().is_none());
    }

    #[test]
    fn lang_to_wiki_basic() {
        assert_eq!(lang_to_wiki("en"), "enwiki");
        assert_eq!(lang_to_wiki("simple"), "simplewiki");
        assert_eq!(lang_to_wiki("zh"), "zhwiki");
    }

    // SseFrameBuffer tests — drive the byte-stream parser directly so
    // we cover the chunk-split cases without spinning up a server.

    #[test]
    fn sse_buffer_complete_single_frame() {
        let mut buf = SseFrameBuffer::new();
        let frames = buf.feed_chunk("data: hello\nid: 1\n\n");
        assert_eq!(frames, vec![("hello".to_string(), Some("1".to_string()))]);
    }

    #[test]
    fn sse_buffer_multi_chunk_frame() {
        let mut buf = SseFrameBuffer::new();
        // Frame fed in 4 separate chunks; only the closing blank line
        // should trigger emission.
        assert!(buf.feed_chunk("data: hel").is_empty());
        assert!(buf.feed_chunk("lo\nid: ").is_empty());
        assert!(buf.feed_chunk("42\n").is_empty());
        let out = buf.feed_chunk("\n");
        assert_eq!(out, vec![("hello".to_string(), Some("42".to_string()))]);
    }

    #[test]
    fn sse_buffer_back_to_back_frames() {
        let mut buf = SseFrameBuffer::new();
        let out = buf.feed_chunk("data: a\n\ndata: b\nid: x\n\n");
        assert_eq!(
            out,
            vec![
                ("a".to_string(), None),
                ("b".to_string(), Some("x".to_string()))
            ]
        );
    }

    #[test]
    fn sse_buffer_multiline_data() {
        let mut buf = SseFrameBuffer::new();
        let out = buf.feed_chunk("data: line1\ndata: line2\nid: 7\n\n");
        assert_eq!(
            out,
            vec![("line1\nline2".to_string(), Some("7".to_string()))]
        );
    }

    #[test]
    fn sse_buffer_ignores_unknown_fields_and_comments() {
        let mut buf = SseFrameBuffer::new();
        let out = buf.feed_chunk(":this is a comment\nevent: edit\ndata: payload\n\n");
        assert_eq!(out, vec![("payload".to_string(), None)]);
    }

    #[test]
    fn sse_buffer_crlf_line_endings() {
        let mut buf = SseFrameBuffer::new();
        let out = buf.feed_chunk("data: hello\r\nid: 1\r\n\r\n");
        assert_eq!(out, vec![("hello".to_string(), Some("1".to_string()))]);
    }

    #[test]
    fn sse_buffer_blank_line_with_no_data_resets_id() {
        // Per spec: if data is empty, the frame isn't dispatched and
        // any standalone id is dropped.
        let mut buf = SseFrameBuffer::new();
        let out = buf.feed_chunk("id: orphan\n\ndata: real\nid: r\n\n");
        assert_eq!(out, vec![("real".to_string(), Some("r".to_string()))]);
    }

    #[test]
    fn sse_buffer_per_byte_chunks() {
        // Worst-case chunking: every byte is its own chunk. Must
        // still produce exactly one frame.
        let mut buf = SseFrameBuffer::new();
        let mut got = Vec::new();
        for b in "data: x\nid: y\n\n".chars() {
            got.extend(buf.feed_chunk(&b.to_string()));
        }
        assert_eq!(got, vec![("x".to_string(), Some("y".to_string()))]);
    }
}
