//! HTML span injection for WhoColor (API.md §7, PLAN.md §4.6).
//!
//! Given Parsoid-rendered HTML and a list of attribution tokens in
//! document order, inject `<span class="editor-token token-editor-X"
//! id="token-N">` wrappers around each token's text in the HTML.
//!
//! The matching strategy mirrors the Python `WikiMarkupParser`
//! (`WhoColor/parser.py:39-58`) at the conceptual level: walk tokens
//! in-order, greedily find the next case-insensitive occurrence of
//! each token's text in the remaining content; tokens whose `str`
//! never matches are silently dropped. The differences:
//!
//! 1. Python operates on the **wikitext**; we operate on the
//!    **rendered HTML** (per PLAN.md §4.6 Option A). Tokens whose
//!    `str` is pure markup (`[[`, `{{`, template names, etc.) won't
//!    appear in the HTML and get silently skipped.
//! 2. Python sends annotated wikitext through MW's `action=parse`
//!    for HTML rendering; we already have Parsoid HTML and never
//!    round-trip back to MW. Faster + WMF's edge cache serves
//!    `/page/html` aggressively.
//!
//! Implementation: parse HTML with [`html5ever`] into an [`RcDom`],
//! flatten the document into a sequence of *events* (markup +
//! text). Concatenate text events into one buffer, run the
//! token-matching cursor against that buffer, then re-emit the
//! events with span wrappers inserted at the recorded match
//! positions. The two-pass structure handles tokens that match in a
//! later text node than the current cursor — see
//! `inject_advances_past_first_text_node_into_second` in tests.

use std::collections::HashMap;
use std::rc::Rc;

use html5ever::interface::QualName;
use html5ever::tendril::TendrilSink;
use html5ever::{LocalName, parse_document};
use markup5ever_rcdom::{Node, NodeData, RcDom};
use md5::{Digest, Md5};

/// Input record for [`inject_spans`]: one token of the requested
/// revision. `class_name` is the final span class — registered
/// editors use the user_id directly, anons use the md5-hashed
/// `"0|<name>"` form (see [`token_class_name`]).
#[derive(Debug, Clone)]
pub struct InjectionToken {
    pub str: String,
    pub editor: String,
    pub class_name: String,
}

/// Output of [`inject_spans`]: the annotated HTML plus the editors
/// present in this revision. Mirrors Python's `present_editors`
/// (`parser.py:223-227`) shape but without percent scaling — the
/// handler converts to the API.md §7 `[name, class_name]` pairs.
#[derive(Debug, Clone, Default)]
pub struct InjectionResult {
    pub html: String,
    pub present_editors: Vec<PresentEditorEntry>,
}

/// One entry in [`InjectionResult::present_editors`]. The handler
/// uses these to build the wire-format `present_editors` array
/// (`[name, class_name]` pairs sorted by token count desc).
#[derive(Debug, Clone)]
pub struct PresentEditorEntry {
    pub editor: String,
    pub class_name: String,
    pub token_count: usize,
}

/// Compute the span class name for a given editor string.
///
/// Per `whocolor/handler.py:108-112`:
/// - Anonymous editors (`editor.starts_with("0|")`) → md5 hex of
///   the editor string.
/// - Everyone else → the editor string unchanged (which is the
///   user_id for registered users).
pub fn token_class_name(editor: &str) -> String {
    if editor.starts_with("0|") {
        let digest = Md5::digest(editor.as_bytes());
        let mut s = String::with_capacity(32);
        for b in digest {
            s.push_str(&format!("{b:02x}"));
        }
        s
    } else {
        editor.to_string()
    }
}

/// Inject `<span>` wrappers around `tokens` in `html`.
///
/// Tokens are matched in order: scan-position only moves forward
/// through the concatenated text content. Tokens that don't appear
/// at all (or only before the current cursor) are silently skipped.
pub fn inject_spans(html: &str, tokens: &[InjectionToken]) -> InjectionResult {
    let dom = parse_document(RcDom::default(), Default::default()).one(html);

    // Pass 1: flatten the DOM into a sequence of events.
    let mut events: Vec<Event> = Vec::new();
    flatten_into_events(&dom.document, &mut events, false);

    // Build the flat text content (concatenation of all Text events
    // that aren't inside_skip), recording each text event's byte
    // range in the flat buffer.
    let mut flat_text = String::new();
    let mut text_ranges: Vec<Option<(usize, usize)>> = Vec::with_capacity(events.len());
    for ev in &events {
        match ev {
            Event::Text { contents, inside_skip: false } => {
                let start = flat_text.len();
                flat_text.push_str(contents);
                text_ranges.push(Some((start, flat_text.len())));
            }
            _ => text_ranges.push(None),
        }
    }

    // Pass 2: match tokens against flat_text in order. For each
    // matched token, record its absolute byte range.
    let (matches, present_editors) = compute_matches(&flat_text, tokens);

    // Pass 3: re-emit events, splicing in spans at match positions.
    let mut out = String::with_capacity(html.len() + matches.len() * 60);
    let mut match_cursor = 0;
    for (ev, range) in events.iter().zip(text_ranges.iter()) {
        match ev {
            Event::Text { contents, inside_skip: true } => {
                push_escaped_text(contents, &mut out);
            }
            Event::Text { contents, inside_skip: false } => {
                let Some((start, end)) = *range else { unreachable!() };
                // Emit this text with any matches that fall in
                // [start, end) inlined as spans.
                emit_text_slice(
                    contents,
                    start,
                    end,
                    &matches,
                    &mut match_cursor,
                    &mut out,
                );
            }
            Event::OpenTag { tag, attrs } => {
                out.push('<');
                out.push_str(tag);
                for (name, value) in attrs {
                    out.push(' ');
                    out.push_str(name);
                    out.push_str("=\"");
                    push_escaped_attr(value, &mut out);
                    out.push('"');
                }
                out.push('>');
            }
            Event::CloseTag { tag } => {
                out.push_str("</");
                out.push_str(tag);
                out.push('>');
            }
            Event::Doctype { name } => {
                out.push_str("<!DOCTYPE ");
                out.push_str(name);
                out.push('>');
            }
            Event::Comment { contents } => {
                out.push_str("<!--");
                out.push_str(contents);
                out.push_str("-->");
            }
        }
    }

    InjectionResult {
        html: out,
        present_editors,
    }
}

/// HTML emission events derived from the parsed DOM. `inside_skip` on
/// text events tracks whether the text came from a skip-element
/// (e.g. `<script>`); the matcher ignores those text spans.
enum Event {
    OpenTag {
        tag: String,
        attrs: Vec<(String, String)>,
    },
    CloseTag {
        tag: String,
    },
    Text {
        contents: String,
        inside_skip: bool,
    },
    Comment {
        contents: String,
    },
    Doctype {
        name: String,
    },
}

/// Walk the DOM in document order, appending `Event`s. Void elements
/// emit only an `OpenTag`. Inside-skip elements still emit their
/// `OpenTag` / children / `CloseTag` — `inside_skip` is propagated
/// onto descendant text events so the matcher knows to skip them.
fn flatten_into_events(node: &Rc<Node>, events: &mut Vec<Event>, inside_skip: bool) {
    match &node.data {
        NodeData::Document => {
            for child in node.children.borrow().iter() {
                flatten_into_events(child, events, inside_skip);
            }
        }
        NodeData::Doctype { name, .. } => {
            events.push(Event::Doctype {
                name: name.to_string(),
            });
        }
        NodeData::Text { contents } => {
            events.push(Event::Text {
                contents: contents.borrow().to_string(),
                inside_skip,
            });
        }
        NodeData::Comment { contents } => {
            events.push(Event::Comment {
                contents: contents.to_string(),
            });
        }
        NodeData::Element { name, attrs, .. } => {
            let tag = name.local.to_string();
            let attr_pairs: Vec<(String, String)> = attrs
                .borrow()
                .iter()
                .map(|a| (format_attr_name(&a.name), a.value.to_string()))
                .collect();
            events.push(Event::OpenTag {
                tag: tag.clone(),
                attrs: attr_pairs,
            });
            if is_void_element(&name.local) {
                return;
            }
            let skip_descendants = inside_skip || is_skip_element(&name.local);
            for child in node.children.borrow().iter() {
                flatten_into_events(child, events, skip_descendants);
            }
            events.push(Event::CloseTag { tag });
        }
        NodeData::ProcessingInstruction { .. } => {}
    }
}

fn format_attr_name(name: &QualName) -> String {
    if let Some(prefix) = &name.prefix {
        format!("{prefix}:{}", name.local)
    } else {
        name.local.to_string()
    }
}

/// A single matched token's location in the flat text buffer.
struct Match {
    start: usize,
    end: usize,
    class_name: String,
    /// The dense id we emit as `id="token-<n>"`. Equals the position
    /// of this match in `matches`.
    id: usize,
}

/// Walk `tokens` in order; for each, find its next case-insensitive
/// occurrence in `flat_text` starting at the current cursor. Each
/// match advances the cursor past the matched bytes. Tokens that
/// don't match anywhere from cursor onward are silently dropped
/// (Python's behavior in `__set_token`).
fn compute_matches(
    flat_text: &str,
    tokens: &[InjectionToken],
) -> (Vec<Match>, Vec<PresentEditorEntry>) {
    let mut matches: Vec<Match> = Vec::new();
    let mut cursor: usize = 0;
    let mut editor_counts: HashMap<String, (String, usize, usize)> = HashMap::new();
    let mut next_order: usize = 0;
    for token in tokens {
        if token.str.is_empty() {
            continue;
        }
        let Some((rel_start, rel_end)) =
            find_case_insensitive(&flat_text[cursor..], &token.str)
        else {
            continue;
        };
        let start = cursor + rel_start;
        let end = cursor + rel_end;
        let id = matches.len();
        matches.push(Match {
            start,
            end,
            class_name: token.class_name.clone(),
            id,
        });
        // Record present_editors.
        let entry = editor_counts
            .entry(token.editor.clone())
            .or_insert((token.class_name.clone(), 0, next_order));
        if entry.2 == next_order {
            next_order += 1;
        }
        entry.1 += 1;
        cursor = clip_to_char_boundary(flat_text, end);
    }

    // Sort present_editors by token_count desc; ties by first-seen
    // order (mirrors how Python presents them — sorted by usage
    // share, with insertion-order tie breaking).
    let mut present_editors: Vec<PresentEditorEntry> = editor_counts
        .into_iter()
        .map(|(editor, (class_name, token_count, _order))| PresentEditorEntry {
            editor,
            class_name,
            token_count,
        })
        .collect();
    let order_map: HashMap<String, usize> = {
        let mut m: HashMap<String, usize> = HashMap::new();
        for (idx, t) in tokens.iter().enumerate() {
            m.entry(t.editor.clone()).or_insert(idx);
        }
        m
    };
    present_editors.sort_by(|a, b| {
        b.token_count
            .cmp(&a.token_count)
            .then_with(|| order_map[&a.editor].cmp(&order_map[&b.editor]))
    });

    (matches, present_editors)
}

/// Emit the text content of one text-event slice, splicing in any
/// matches whose byte range falls within `[event_start, event_end)`.
fn emit_text_slice(
    contents: &str,
    event_start: usize,
    event_end: usize,
    matches: &[Match],
    cursor: &mut usize,
    out: &mut String,
) {
    // Local cursor within this text event's contents (byte index).
    let mut local: usize = 0;
    while *cursor < matches.len() {
        let m = &matches[*cursor];
        if m.start >= event_end {
            // Match belongs to a later event; emit the rest of this
            // event verbatim and bail.
            break;
        }
        if m.end <= event_start {
            // Match already past — shouldn't happen if matches are in
            // order, but be defensive.
            *cursor += 1;
            continue;
        }
        // Compute local positions.
        let m_local_start = m.start.saturating_sub(event_start);
        let m_local_end = m.end.saturating_sub(event_start).min(contents.len());
        // Emit text before this match.
        if local < m_local_start {
            push_escaped_text(&contents[local..m_local_start], out);
        }
        // Emit the span.
        out.push_str("<span class=\"editor-token token-editor-");
        push_escaped_attr(&m.class_name, out);
        out.push_str("\" id=\"token-");
        out.push_str(&m.id.to_string());
        out.push_str("\">");
        push_escaped_text(&contents[m_local_start..m_local_end], out);
        out.push_str("</span>");
        local = m_local_end;
        *cursor += 1;
    }
    // Trailing text after the last match in this event (or all of it
    // if no matches).
    if local < contents.len() {
        push_escaped_text(&contents[local..], out);
    }
    // Sanity: silence unused warning.
    let _ = event_end;
}

/// HTML5 elements whose contents we pass through verbatim — token
/// matching skips into them.
fn is_skip_element(tag: &LocalName) -> bool {
    matches!(
        tag.as_ref(),
        "script" | "style" | "head" | "noscript" | "template"
    )
}

/// HTML5 void elements — emit without a closing tag. Not exhaustive;
/// covers the ones Parsoid commonly emits.
fn is_void_element(tag: &LocalName) -> bool {
    matches!(
        tag.as_ref(),
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// Find `needle` inside `haystack` case-insensitively, returning the
/// `(start, end)` byte range of the match in `haystack`. Matches the
/// Python `re.search(re.escape(token), text, re.IGNORECASE)`
/// semantics — substring match, no word boundaries.
///
/// `start` and `end` are always char boundaries in `haystack`. The
/// matched substring may have a different byte length than `needle`
/// because case-folding can change byte length (e.g. Turkish "İ"
/// lowercases to "i̇", which is two code points / three bytes).
/// Returning the actual byte range avoids landing the cursor inside
/// a multi-byte sequence.
///
/// Bounded-cost O(n*m): we iterate char positions in `haystack` and
/// accumulate lowercase chars until we've seen enough to compare
/// against `needle.to_lowercase()`.
fn find_case_insensitive(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return Some((0, 0));
    }
    let n_lower: String = needle.to_lowercase();
    let n_lower_bytes = n_lower.len();

    for (start_idx, _) in haystack.char_indices() {
        let mut buf = String::new();
        let mut last_consumed_end = start_idx;
        for (rel, ch) in haystack[start_idx..].char_indices() {
            for lc in ch.to_lowercase() {
                buf.push(lc);
            }
            last_consumed_end = start_idx + rel + ch.len_utf8();
            if buf.len() >= n_lower_bytes {
                break;
            }
        }
        if buf.starts_with(&n_lower) {
            return Some((start_idx, last_consumed_end));
        }
    }
    None
}

/// Move `byte_idx` forward to the next char boundary if it lands
/// inside a multi-byte UTF-8 sequence. Conservative: returns the
/// smallest valid boundary at-or-after `byte_idx`.
fn clip_to_char_boundary(s: &str, byte_idx: usize) -> usize {
    let mut i = byte_idx;
    while i <= s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i.min(s.len())
}

/// Append text to `out`, escaping the four HTML-significant
/// characters and converting non-breaking space to an entity.
fn push_escaped_text(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            _ => out.push(c),
        }
    }
}

/// Same as [`push_escaped_text`] but for attribute values: also
/// escapes the double quote.
fn push_escaped_attr(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            _ => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str, editor: &str) -> InjectionToken {
        InjectionToken {
            str: s.to_string(),
            editor: editor.to_string(),
            class_name: token_class_name(editor),
        }
    }

    #[test]
    fn class_name_for_registered_is_user_id() {
        assert_eq!(token_class_name("12345"), "12345");
    }

    #[test]
    fn class_name_for_anon_is_md5_hex() {
        let class = token_class_name("0|Anon");
        assert_eq!(class.len(), 32);
        assert!(class.chars().all(|c| c.is_ascii_hexdigit()));
        // Stability check.
        assert_eq!(class, token_class_name("0|Anon"));
        assert_ne!(class, token_class_name("0|Other"));
    }

    #[test]
    fn inject_wraps_simple_text_tokens() {
        let html = "<p>Hello world</p>";
        let tokens = vec![t("hello", "1"), t("world", "2")];
        let r = inject_spans(html, &tokens);
        assert!(r.html.contains(
            "<span class=\"editor-token token-editor-1\" id=\"token-0\">Hello</span>"
        ));
        assert!(r.html.contains(
            "<span class=\"editor-token token-editor-2\" id=\"token-1\">world</span>"
        ));
        assert_eq!(r.present_editors.len(), 2);
        assert!(r.present_editors.iter().all(|e| e.token_count == 1));
    }

    #[test]
    fn inject_is_case_insensitive() {
        let html = "<p>HELLO World</p>";
        let r = inject_spans(html, &[t("hello", "1"), t("world", "2")]);
        assert!(r.html.contains(">HELLO</span>"));
        assert!(r.html.contains(">World</span>"));
    }

    #[test]
    fn inject_skips_tokens_not_in_html() {
        // `[[` and `]]` are wikitext markup with no HTML rendering.
        // The injector should drop them on the floor.
        let html = "<p>Hello world</p>";
        let tokens = vec![
            t("[[", "1"),
            t("hello", "1"),
            t("]]", "1"),
            t("world", "1"),
        ];
        let r = inject_spans(html, &tokens);
        assert!(
            r.html.contains("id=\"token-0\">Hello</span>"),
            "expected Hello wrapped with id 0, got: {}",
            r.html
        );
        assert!(r.html.contains("id=\"token-1\">world</span>"));
        // One editor, two matched tokens.
        assert_eq!(r.present_editors.len(), 1);
        assert_eq!(r.present_editors[0].token_count, 2);
    }

    #[test]
    fn inject_advances_past_first_text_node_into_second() {
        // First text node: "Hello ", second: "world". `world` must
        // match in the second text node.
        let html = "<p>Hello <b>world</b></p>";
        let r = inject_spans(html, &[t("hello", "1"), t("world", "2")]);
        assert!(r.html.contains("<b><span"), "got: {}", r.html);
        assert!(r.html.contains("token-editor-2\" id=\"token-1\">world</span>"));
    }

    #[test]
    fn inject_inside_script_is_skipped() {
        let html = "<script>var x = 'hello';</script><p>Hello world</p>";
        let r = inject_spans(html, &[t("hello", "1"), t("world", "2")]);
        // The 'hello' inside <script> must not get wrapped.
        assert!(
            r.html.contains("<script>var x = 'hello';</script>"),
            "script content preserved verbatim, got: {}",
            r.html
        );
        assert!(r.html.contains("id=\"token-0\">Hello</span>"));
        assert!(r.html.contains("id=\"token-1\">world</span>"));
    }

    #[test]
    fn inject_preserves_existing_attributes() {
        let html = r#"<p class="lead">Hello</p>"#;
        let r = inject_spans(html, &[t("hello", "1")]);
        assert!(r.html.contains(r#"<p class="lead">"#));
        assert!(r.html.contains(">Hello</span>"));
    }

    #[test]
    fn inject_handles_void_elements() {
        let html = r#"<p>Hello<br>world</p>"#;
        let r = inject_spans(html, &[t("hello", "1"), t("world", "2")]);
        assert!(r.html.contains("<br>"));
        assert!(!r.html.contains("</br>"));
        assert!(r.html.contains("Hello</span><br><span"));
    }

    #[test]
    fn inject_unicode_token_matches() {
        let html = "<p>中国是一个国家</p>";
        let r = inject_spans(html, &[t("中国", "1"), t("国家", "1")]);
        assert!(r.html.contains(">中国</span>"));
        assert!(r.html.contains(">国家</span>"));
        assert_eq!(r.present_editors.len(), 1);
        assert_eq!(r.present_editors[0].token_count, 2);
    }

    #[test]
    fn inject_present_editors_sorted_by_count_desc() {
        // a (×3) by editor 1; b (×2) by editor 2; c (×1) by 3; d (×1)
        // by 4.
        let html = "<p>a b c d a a b</p>";
        let tokens = vec![
            t("a", "1"), t("b", "2"), t("c", "3"), t("d", "4"),
            t("a", "1"), t("a", "1"), t("b", "2"),
        ];
        let r = inject_spans(html, &tokens);
        assert!(r.present_editors.len() >= 3);
        assert_eq!(r.present_editors[0].editor, "1");
        assert_eq!(r.present_editors[0].token_count, 3);
        assert_eq!(r.present_editors[1].editor, "2");
        assert_eq!(r.present_editors[1].token_count, 2);
    }

    #[test]
    fn inject_escapes_html_in_tokens() {
        // Source HTML has `&lt;b&gt;` — html5ever decodes to literal
        // `<b>` in the text node. When we re-emit, the special
        // chars must be re-escaped.
        let html = "<p>&lt;b&gt;</p>";
        let r = inject_spans(html, &[t("<b>", "1")]);
        assert!(r.html.contains("&lt;b&gt;"));
        // The span should wrap the (decoded then re-escaped) match.
        assert!(r.html.contains("<span"));
    }
}
