//! Wikitext-level span injection for WhoColor.
//!
//! Ports `WhoColor.parser.WikiMarkupParser` from the production
//! reference implementation (`../wikiwho_api/env/lib/python3.9/
//! site-packages/WhoColor/parser.py` + `special_markups.py`). The
//! handler invokes [`inject_spans_into_wikitext`] to wrap algorithm
//! tokens in `<span class="editor-token token-editor-{class}"
//! id="token-{n}">…</span>` markup at their byte positions in the
//! wikitext, then POSTs the modified wikitext through MW Action API
//! `action=parse` to render the final HTML.
//!
//! This is the path to byte-for-byte WhoColor parity with
//! production. HTML-level injection in `whocolor_html` exists as a
//! fallback / for future smart-extractor work; the production flow
//! is wikitext-level only.
//!
//! ## Why wikitext-level injection?
//!
//! Tokens in the WikiWho algorithm come from a wikitext tokenizer.
//! Many of those tokens are inside special wikitext constructs
//! (templates, references, link targets, magic words) that the MW
//! parser expands or consumes during the wikitext-to-HTML render.
//! If we tried to match algorithm tokens against rendered HTML
//! text, we'd find at best a small fraction (~3% in the first
//! WMCloud measurement). By injecting at wikitext byte positions
//! and letting MW carry the spans through, every token survives by
//! construction.
//!
//! ## Caveats inherited from the Python reference
//!
//! - The `[\\*#\\:]*;` list-prefix regex in `special_markups.py`
//!   includes literal backslashes in the character class. This is
//!   likely a typo in the upstream code; we mirror it verbatim for
//!   parity.
//! - Newlines are substituted with the `WIKICOLORLB` placeholder
//!   before processing and restored after, so regex matching works
//!   across multi-line constructs without DOTALL semantics.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

/// Placeholder used in place of `\n` (and `\r\n` / `\r`) during
/// parsing, restored afterward. Matches the upstream constant in
/// `WhoColor/special_markups.py`.
const REGEX_HELPER_PATTERN: &str = "WIKICOLORLB";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkupKind {
    /// Has a start regex and an end regex; the markup spans the
    /// range from start to end inclusive.
    Block,
    /// Has only a start regex; the markup is just the start match.
    Single,
}

struct SpecialMarkup {
    kind: MarkupKind,
    start: Regex,
    end: Option<Regex>,
    /// If true, tokens inside this markup do not get span-wrapped.
    /// (Templates, refs, math, comments, etc.)
    no_spans: bool,
    /// If true, the recursive parser does not look for further
    /// special markups inside this one. Used for single-element
    /// markups like `<nowiki/>` and magic words.
    no_jump: bool,
}

/// The list mirrors `WhoColor.special_markups.SPECIAL_MARKUPS` in
/// order. Order matters because the parser picks the
/// earliest-starting markup when multiple match at different
/// positions — but the first match by *position* across the list,
/// not the first by list order. See `find_next_special_markup`.
static SPECIAL_MARKUPS: LazyLock<Vec<SpecialMarkup>> = LazyLock::new(|| {
    vec![
        // Internal wiki links: [[ ]]
        SpecialMarkup {
            kind: MarkupKind::Block,
            start: Regex::new(r"\[\[").unwrap(),
            end: Some(Regex::new(r"\]\]").unwrap()),
            no_spans: false,
            no_jump: false,
        },
        // External links: [ ]
        SpecialMarkup {
            kind: MarkupKind::Block,
            start: Regex::new(r"\[").unwrap(),
            end: Some(Regex::new(r"\]").unwrap()),
            no_spans: false,
            no_jump: false,
        },
        // Templates: {{ }}
        SpecialMarkup {
            kind: MarkupKind::Block,
            start: Regex::new(r"\{\{").unwrap(),
            end: Some(Regex::new(r"\}\}").unwrap()),
            no_spans: true,
            no_jump: false,
        },
        // Self-closing <nowiki/>
        SpecialMarkup {
            kind: MarkupKind::Single,
            start: Regex::new(r"(<nowiki */>)").unwrap(),
            end: None,
            no_spans: true,
            no_jump: true,
        },
        // Math, timeline, nowiki tags (block)
        SpecialMarkup {
            kind: MarkupKind::Block,
            start: Regex::new(r"<(math|timeline|nowiki)[^>]*>").unwrap(),
            end: Some(Regex::new(r"</(math|timeline|nowiki)>").unwrap()),
            no_spans: true,
            no_jump: false,
        },
        // General HTML opening/closing tag — body of the tag (between
        // `<tag` and `>`).
        SpecialMarkup {
            kind: MarkupKind::Block,
            start: Regex::new(
                r"</?(ref|h1|h2|h3|h4|h5|h6|p|br|hr|!--|abbr|b|bdi|bdo|blockquote|cite|code|data|del|dfn|em|i|ins|kbd|mark|pre|q|ruby|rt|rp|s|samp|small|strong|sub|sup|time|u|var|wbr|dl|dt|dd|ol|ul|li|div|span|table|tr|td|th|caption)",
            )
            .unwrap(),
            end: Some(Regex::new(r">").unwrap()),
            no_spans: true,
            no_jump: false,
        },
        // Headings: =+ or ;
        SpecialMarkup {
            kind: MarkupKind::Single,
            start: Regex::new(r"(=+|;)").unwrap(),
            end: None,
            no_spans: true,
            no_jump: true,
        },
        // Lists and blocks: list-prefix followed by ; up to next :
        // (literal backslash inclusion mirrors upstream, see module
        // docs).
        SpecialMarkup {
            kind: MarkupKind::Block,
            start: Regex::new(r"[\\*#\\:]*;").unwrap(),
            end: Some(Regex::new(r"\\:").unwrap()),
            no_spans: true,
            no_jump: false,
        },
        // List bullets
        SpecialMarkup {
            kind: MarkupKind::Single,
            start: Regex::new(r"[\\*#:]+").unwrap(),
            end: None,
            no_spans: true,
            no_jump: true,
        },
        // Horizontal lines (4+ dashes — first char in pattern is `-----*`,
        // i.e. 5+ literal dashes — the Python intent appears to be "4 or
        // more", but it ports as-is)
        SpecialMarkup {
            kind: MarkupKind::Single,
            start: Regex::new(r"-----*").unwrap(),
            end: None,
            no_spans: true,
            no_jump: true,
        },
        // Tables: {| |}
        SpecialMarkup {
            kind: MarkupKind::Block,
            start: Regex::new(r"\{\|").unwrap(),
            end: Some(Regex::new(r"\|\}").unwrap()),
            no_spans: true,
            no_jump: false,
        },
        // Linebreaks — runs of the helper pattern
        SpecialMarkup {
            kind: MarkupKind::Single,
            start: Regex::new(&format!(r"({REGEX_HELPER_PATTERN})+")).unwrap(),
            end: None,
            no_spans: true,
            no_jump: true,
        },
        // HTML entities
        SpecialMarkup {
            kind: MarkupKind::Single,
            start: Regex::new(
                r"(&nbsp;|&euro;|&quot;|&amp;|&lt;|&gt;|&nbsp;|&(?:[a-z\d]+|#\d+|#x[a-f\d]+);)",
            )
            .unwrap(),
            end: None,
            no_spans: true,
            no_jump: true,
        },
        // Magic words
        SpecialMarkup {
            kind: MarkupKind::Single,
            start: Regex::new(
                r"__(NOTOC|FORCETOC|TOC|NOEDITSECTION|NEWSECTIONLINK|NONEWSECTIONLINK|NOGALLERY|HIDDENCAT|NOCONTENTCONVERT|NOCC|NOTITLECONVERT|NOTC|START|END|INDEX|NOINDEX|STATICREDIRECT|DISAMBIG)__",
            )
            .unwrap(),
            end: None,
            no_spans: true,
            no_jump: true,
        },
        // Apostrophes for italics/bold
        SpecialMarkup {
            kind: MarkupKind::Single,
            start: Regex::new(r"''+").unwrap(),
            end: None,
            no_spans: true,
            no_jump: true,
        },
    ]
});

#[derive(Debug, Clone)]
pub struct WikitextToken {
    /// Token string as the algorithm tokenized it (lowercased per
    /// the WikiWho convention).
    pub str: String,
    /// Editor — either a user_id string for registered users or
    /// `0|<ip>` for anons.
    pub editor: String,
    /// Class name used in the span attribute — user_id for
    /// registered users, md5(editor) for anons. See
    /// `whocolor_html::token_class_name`.
    pub class_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentEditorEntry {
    pub editor: String,
    pub class_name: String,
    pub token_count: usize,
}

pub struct InjectionResult {
    pub wikitext: String,
    pub present_editors: Vec<PresentEditorEntry>,
}

/// Walk `wikitext` in algorithm-token order, injecting span markup
/// at each token's byte position (subject to the special-markup
/// rules). Returns the modified wikitext plus a present_editors
/// roster sorted by token count desc, with first-seen ordering as
/// the tie breaker.
pub fn inject_spans_into_wikitext(
    wikitext: &str,
    tokens: &[WikitextToken],
) -> InjectionResult {
    let mut parser = Parser::new(wikitext, tokens);
    parser.run();
    parser.finish()
}

struct Parser<'a> {
    /// Owned because we substitute `\n`/`\r\n`/`\r` with the helper
    /// pattern before parsing and restore afterward.
    wiki_text: String,
    tokens: &'a [WikitextToken],
    token_index: usize,
    /// End position (exclusive) of the current token in `wiki_text`.
    /// `None` once we've exhausted matchable tokens.
    current_token_end: Option<usize>,
    wiki_text_pos: usize,
    open_span: bool,
    /// Start positions of special-markup elements we've already
    /// descended into. Prevents infinite recursion when an outer
    /// match reappears as the earliest match on re-scan.
    jumped_elems: HashSet<usize>,
    out: String,
    /// editor → (class_name, token_count, insertion_order)
    present_editors: HashMap<String, EditorState>,
}

#[derive(Clone)]
struct SpecialElem {
    markup_idx: usize,
    start: usize,
    start_len: usize,
}

#[derive(Clone)]
struct SpecialElemEnd {
    start: usize,
    end: usize,
}

struct EditorState {
    class_name: String,
    count: usize,
    insertion_order: usize,
}

impl<'a> Parser<'a> {
    fn new(wikitext: &str, tokens: &'a [WikitextToken]) -> Self {
        let wiki_text = wikitext
            .replace("\r\n", REGEX_HELPER_PATTERN)
            .replace(['\n', '\r'], REGEX_HELPER_PATTERN);
        Self {
            wiki_text,
            tokens,
            token_index: 0,
            current_token_end: None,
            wiki_text_pos: 0,
            open_span: false,
            jumped_elems: HashSet::new(),
            out: String::with_capacity(wikitext.len() + tokens.len() * 60),
            present_editors: HashMap::new(),
        }
    }

    fn run(&mut self) {
        self.set_token();
        self.parse_wiki_text(true, None, false);
    }

    fn finish(self) -> InjectionResult {
        let wikitext = self.out.replace(REGEX_HELPER_PATTERN, "\n");
        let mut editors: Vec<(String, EditorState)> = self.present_editors.into_iter().collect();
        editors.sort_by(|a, b| {
            b.1.count
                .cmp(&a.1.count)
                .then_with(|| a.1.insertion_order.cmp(&b.1.insertion_order))
        });
        let present_editors = editors
            .into_iter()
            .map(|(editor, st)| PresentEditorEntry {
                editor,
                class_name: st.class_name,
                token_count: st.count,
            })
            .collect();
        InjectionResult {
            wikitext,
            present_editors,
        }
    }

    /// Advance `token_index` to the next token whose `str` can be
    /// found in `wiki_text[wiki_text_pos..]` (case-insensitive),
    /// recording its end position. Tokens whose str isn't findable
    /// at all (e.g. due to case-folding length quirks like
    /// `İstanbul`) are silently skipped — mirrors the Python
    /// reference's `__set_token`.
    fn set_token(&mut self) {
        while self.token_index < self.tokens.len() {
            let token = &self.tokens[self.token_index];
            if let Some(end) =
                find_case_insensitive_end(&self.wiki_text, self.wiki_text_pos, &token.str)
            {
                self.current_token_end = Some(end);
                // Register the editor (mirrors Python's set_token,
                // which counts every token's editor even if the
                // token is later span-suppressed inside a template).
                let order = self.present_editors.len();
                let entry =
                    self.present_editors
                        .entry(token.editor.clone())
                        .or_insert(EditorState {
                            class_name: token.class_name.clone(),
                            count: 0,
                            insertion_order: order,
                        });
                entry.count += 1;
                return;
            }
            self.token_index += 1;
        }
        self.current_token_end = None;
    }

    /// Find the next special-markup start position at or after
    /// `wiki_text_pos`, skipping any positions already in
    /// `jumped_elems`. Returns `None` if no markup matches.
    fn find_next_special_markup(&self) -> Option<SpecialElem> {
        let mut best: Option<SpecialElem> = None;
        for (idx, markup) in SPECIAL_MARKUPS.iter().enumerate() {
            if let Some(m) = markup.start.find(&self.wiki_text[self.wiki_text_pos..]) {
                let abs_start = self.wiki_text_pos + m.start();
                if self.jumped_elems.contains(&abs_start) {
                    continue;
                }
                let candidate = SpecialElem {
                    markup_idx: idx,
                    start: abs_start,
                    start_len: m.end() - m.start(),
                };
                match &best {
                    Some(b) if b.start <= candidate.start => {}
                    _ => best = Some(candidate),
                }
            }
        }
        best
    }

    /// Compute the end position of the given special element.
    /// For a Single markup, end == start + start_len. For a Block
    /// markup, scans forward FROM THE CURRENT CURSOR for the end
    /// regex; if no end match is found, treats the markup as
    /// ending at end-of-wikitext.
    ///
    /// The cursor-relative search is load-bearing: when called
    /// inside a recursion after a nested markup has been
    /// processed, `wiki_text_pos` is past the nested markup's end,
    /// so the search picks up the OUTER markup's `}}` rather than
    /// the (already-processed) nested one's. Mirrors Python's
    /// `WikiMarkupParser.__get_special_elem_end` which uses
    /// `_wiki_text_pos`.
    fn special_elem_end(&self, se: &SpecialElem) -> SpecialElemEnd {
        let markup = &SPECIAL_MARKUPS[se.markup_idx];
        match markup.kind {
            MarkupKind::Single => SpecialElemEnd {
                start: se.start,
                end: se.start + se.start_len,
            },
            MarkupKind::Block => {
                let end_regex = markup
                    .end
                    .as_ref()
                    .expect("block markup must have end regex");
                // Search from the current cursor — not from
                // `se.start + se.start_len` — so already-consumed
                // nested-markup ends don't get returned again.
                let search_from = self.wiki_text_pos;
                if search_from >= self.wiki_text.len() {
                    return SpecialElemEnd {
                        start: self.wiki_text.len(),
                        end: self.wiki_text.len(),
                    };
                }
                if let Some(m) = end_regex.find(&self.wiki_text[search_from..]) {
                    let abs_start = search_from + m.start();
                    SpecialElemEnd {
                        start: abs_start,
                        end: abs_start + (m.end() - m.start()),
                    }
                } else {
                    // Unclosed block; treat the rest of the wikitext
                    // as the element body.
                    SpecialElemEnd {
                        start: self.wiki_text.len(),
                        end: self.wiki_text.len(),
                    }
                }
            }
        }
    }

    fn add_spans(&mut self, token_class_name: &str, new_span: bool) {
        if self.open_span {
            self.out.push_str("</span>");
            self.open_span = false;
        }
        if new_span {
            self.out.push_str("<span class=\"editor-token token-editor-");
            push_escaped_attr(token_class_name, &mut self.out);
            self.out.push_str("\" id=\"token-");
            self.out.push_str(&self.token_index.to_string());
            self.out.push_str("\">");
            self.open_span = true;
        }
    }

    /// Core parse loop. Mirrors `WhoColor.parser.WikiMarkupParser.
    /// __parse_wiki_text`. `add_spans` toggles span wrapping;
    /// `special_elem` carries the outer-markup context for the
    /// recursive call; `no_jump` disables descent into further
    /// markups (used for `single` markups).
    fn parse_wiki_text(
        &mut self,
        add_spans: bool,
        special_elem: Option<SpecialElem>,
        no_jump: bool,
    ) {
        let mut special_elem_end: Option<SpecialElemEnd> =
            special_elem.as_ref().map(|se| self.special_elem_end(se));
        let mut next_special: Option<SpecialElem> = if no_jump {
            None
        } else {
            self.find_next_special_markup()
        };

        while self.wiki_text_pos < self.wiki_text.len() {
            let Some(token_end) = self.current_token_end else {
                // No token left — flush remaining text.
                self.out.push_str(&self.wiki_text[self.wiki_text_pos..]);
                self.wiki_text_pos = self.wiki_text.len();
                if self.open_span {
                    self.out.push_str("</span>");
                    self.open_span = false;
                }
                return;
            };

            // Should we descend into a nested special markup?
            if !no_jump {
                let outer_end_far_enough = match &special_elem_end {
                    None => true,
                    Some(see) => self.wiki_text_pos < see.start,
                };
                if outer_end_far_enough {
                    if let Some(nse) = next_special.clone() {
                        let nse_before_outer_end = match &special_elem_end {
                            None => true,
                            Some(see) => nse.start < see.start,
                        };
                        if nse_before_outer_end && nse.start < token_end {
                            // Jump in.
                            self.jumped_elems.insert(nse.start);
                            let markup_idx = nse.markup_idx;
                            let markup_no_spans = SPECIAL_MARKUPS[markup_idx].no_spans;
                            let markup_no_jump = SPECIAL_MARKUPS[markup_idx].no_jump;
                            if add_spans {
                                let class_name =
                                    self.tokens[self.token_index].class_name.clone();
                                self.add_spans(&class_name, !markup_no_spans);
                            }
                            self.parse_wiki_text(false, Some(nse), markup_no_jump);
                            // After return, re-derive both helpers.
                            special_elem_end = special_elem
                                .as_ref()
                                .map(|se| self.special_elem_end(se));
                            next_special = self.find_next_special_markup();
                            continue;
                        }
                    }
                }
            }

            // Is the outer special element's end reached before this
            // token's end? If so, flush up to that boundary and
            // return — the caller (the next stack frame up) takes
            // over.
            if let Some(see) = &special_elem_end {
                if see.end < token_end {
                    self.out
                        .push_str(&self.wiki_text[self.wiki_text_pos..see.end]);
                    self.wiki_text_pos = see.end;
                    return;
                }
            }

            // Normal token: write text up to token_end (optionally
            // wrapped in a span), then advance.
            if add_spans {
                let class_name = self.tokens[self.token_index].class_name.clone();
                self.add_spans(&class_name, true);
            }
            self.out
                .push_str(&self.wiki_text[self.wiki_text_pos..token_end]);
            self.wiki_text_pos = token_end;
            self.token_index += 1;
            self.set_token();
        }

        // End-of-buffer cleanup.
        if self.open_span {
            self.out.push_str("</span>");
            self.open_span = false;
        }
    }
}

/// Case-insensitive substring search starting at `start` in
/// `haystack`, returning the **end byte position** of the match in
/// `haystack` (not in the slice). `None` if not found.
fn find_case_insensitive_end(haystack: &str, start: usize, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    if start >= haystack.len() {
        return None;
    }
    let hay_slice = &haystack[start..];
    // Fast path: all-ASCII case-insensitive byte compare.
    if needle.is_ascii() && hay_slice.is_ascii() {
        let hay = hay_slice.as_bytes();
        let nee = needle.as_bytes();
        if nee.len() > hay.len() {
            return None;
        }
        for i in 0..=(hay.len() - nee.len()) {
            if hay[i..i + nee.len()]
                .iter()
                .zip(nee.iter())
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
            {
                return Some(start + i + nee.len());
            }
        }
        return None;
    }
    // Slow path: Unicode case-fold comparison via to_lowercase().
    let nee_lower = needle.to_lowercase();
    let hay_lower = hay_slice.to_lowercase();
    if let Some(rel) = hay_lower.find(&nee_lower) {
        // Map the position back to the original-case haystack.
        // hay_slice and hay_lower may differ in byte length when
        // case folding changes lengths (e.g. İ → i̇). We can't
        // generally recover the original-text end position from
        // the lower-text position. As a pragmatic fallback, look
        // for a length-aligned match in the original by linear
        // scan.
        let needle_byte_len_in_original = scan_unicode_match(hay_slice, &nee_lower)?;
        let _ = rel;
        return Some(start + needle_byte_len_in_original);
    }
    None
}

/// Linear scan for a case-insensitive Unicode match. Returns the
/// end byte offset (in `hay`) of the first match. Used only on the
/// rare non-ASCII path.
fn scan_unicode_match(hay: &str, needle_lower: &str) -> Option<usize> {
    let hay_len = hay.len();
    let mut start = 0usize;
    while start < hay_len {
        for end in (start + 1)..=hay_len {
            if !hay.is_char_boundary(end) {
                continue;
            }
            let candidate = &hay[start..end];
            if candidate.to_lowercase() == needle_lower {
                return Some(end);
            }
            if candidate.to_lowercase().len() > needle_lower.len() {
                break;
            }
        }
        // Advance to next char boundary.
        start += hay[start..]
            .chars()
            .next()
            .map(|c| c.len_utf8())
            .unwrap_or(1);
    }
    None
}

fn push_escaped_attr(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '"' => out.push_str("&quot;"),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str, editor: &str, class_name: &str) -> WikitextToken {
        WikitextToken {
            str: s.into(),
            editor: editor.into(),
            class_name: class_name.into(),
        }
    }

    #[test]
    fn empty_wikitext_yields_empty_output() {
        let out = inject_spans_into_wikitext("", &[]);
        assert_eq!(out.wikitext, "");
        assert!(out.present_editors.is_empty());
    }

    #[test]
    fn plain_text_gets_spans_per_token() {
        // Wikitext: "hello world"
        // Tokens (lowercased): "hello", " ", "world"
        let toks = vec![
            t("hello", "42", "42"),
            t("world", "42", "42"),
        ];
        let out = inject_spans_into_wikitext("hello world", &toks);
        assert!(
            out.wikitext.contains("<span class=\"editor-token token-editor-42\" id=\"token-0\">hello</span>"),
            "missing span for first token: {}",
            out.wikitext
        );
        assert!(
            out.wikitext.contains("token-editor-42\" id=\"token-1\""),
            "missing span for second token: {}",
            out.wikitext
        );
    }

    #[test]
    fn template_tokens_are_not_span_wrapped() {
        // {{cite|args}} — tokens inside the template should not be
        // span-wrapped (no_spans=true on Template markup).
        let toks = vec![
            t("hello", "42", "42"),
            t("cite", "99", "99"),  // inside template
            t("args", "99", "99"),  // inside template
            t("world", "42", "42"),
        ];
        let out = inject_spans_into_wikitext("hello {{cite|args}} world", &toks);
        // Should have spans for "hello" and "world" but NOT inside {{...}}
        let html = &out.wikitext;
        assert!(html.contains("token-editor-42\" id=\"token-0\">hello</span>"), "{html}");
        // The template body shouldn't contain a token-editor span:
        let template_start = html.find("{{").expect("template should remain");
        let template_end = html.find("}}").expect("template close should remain");
        let template_body = &html[template_start..template_end + 2];
        assert!(
            !template_body.contains("token-editor"),
            "template body should not have spans, got: {template_body}"
        );
        // "world" should be wrapped — leading whitespace gets
        // absorbed into the span (matches production: leading
        // space before a token is included in that token's span).
        assert!(
            html.contains("> world</span>"),
            "world should be span-wrapped (with leading space): {html}"
        );
    }

    #[test]
    fn internal_link_target_excluded_from_spans() {
        // [[Foo]] — tokens inside [[ ]] do not get span-wrapped
        // because the Internal-link special markup is matched.
        // Note: in the WhoColor convention, [[...]] is a special
        // markup but with no_spans=false at the start (the first
        // token of the link can still get its span), and recursion
        // descends inside to look for more markup. The plain text
        // "Foo" inside [[Foo]] is treated like any other text — but
        // because the descent uses add_spans=False, no spans are
        // added inside.
        let toks = vec![
            t("see", "42", "42"),
            t("foo", "99", "99"),  // inside link
            t("end", "42", "42"),
        ];
        let out = inject_spans_into_wikitext("see [[foo]] end", &toks);
        let html = &out.wikitext;
        // "see" should have a span.
        assert!(html.contains(">see</span>"), "see should be wrapped: {html}");
        // "end" should have a span — leading space absorbed.
        assert!(html.contains("> end</span>"), "end should be wrapped: {html}");
        // Link body shouldn't have an internal editor-token span.
        let link_start = html.find("[[").unwrap();
        let link_end = html.find("]]").unwrap();
        let link_body = &html[link_start..link_end + 2];
        assert!(
            !link_body.contains("token-editor-99"),
            "link body should not contain inner span, got: {link_body}"
        );
    }

    #[test]
    fn newlines_round_trip_through_helper_substitution() {
        let toks = vec![t("hello", "1", "1"), t("world", "1", "1")];
        let out = inject_spans_into_wikitext("hello\nworld", &toks);
        assert!(
            out.wikitext.contains('\n'),
            "newline should round-trip back: {}",
            out.wikitext
        );
        assert!(!out.wikitext.contains(REGEX_HELPER_PATTERN));
    }

    #[test]
    fn present_editors_sorted_by_count_desc_then_first_seen() {
        let toks = vec![
            t("alpha", "1", "1"),  // first-seen: editor 1
            t("beta", "2", "2"),   // first-seen: editor 2
            t("gamma", "1", "1"),  // editor 1 count = 2
            t("delta", "3", "3"),  // first-seen: editor 3
            t("epsilon", "2", "2"), // editor 2 count = 2
        ];
        let out = inject_spans_into_wikitext("alpha beta gamma delta epsilon", &toks);
        // Sorted by count desc, ties by insertion order.
        // counts: 1→2, 2→2, 3→1. Tie 1 vs 2 broken by 1 seen first.
        assert_eq!(out.present_editors.len(), 3);
        assert_eq!(out.present_editors[0].editor, "1");
        assert_eq!(out.present_editors[0].token_count, 2);
        assert_eq!(out.present_editors[1].editor, "2");
        assert_eq!(out.present_editors[1].token_count, 2);
        assert_eq!(out.present_editors[2].editor, "3");
        assert_eq!(out.present_editors[2].token_count, 1);
    }

    #[test]
    fn case_insensitive_match() {
        // Wikitext "Hello", lowercase token "hello".
        let toks = vec![t("hello", "42", "42")];
        let out = inject_spans_into_wikitext("Hello", &toks);
        assert!(
            out.wikitext.contains(">Hello</span>"),
            "should match Hello vs hello: {}",
            out.wikitext
        );
    }

    #[test]
    fn ref_tag_body_excluded_from_spans() {
        // <ref>citation</ref> — the General HTML tag block markup
        // covers the start of the ref tag (`<ref` through `>`).
        // The text between <ref> and </ref> may still get tokens —
        // but the tag attributes themselves shouldn't.
        let toks = vec![
            t("body", "42", "42"),
            t("ref", "99", "99"),
            t("text", "99", "99"),
        ];
        let out = inject_spans_into_wikitext("body<ref>text</ref>", &toks);
        // "body" should wrap.
        assert!(out.wikitext.contains(">body</span>"));
    }

    #[test]
    fn tokens_appearing_only_after_other_match_are_skipped() {
        // If a token's str doesn't appear after the cursor, it gets
        // skipped (Python's set_token loops past unmatchable tokens).
        let toks = vec![
            t("foo", "1", "1"),
            t("baz", "1", "1"), // never appears; should skip cleanly
            t("bar", "1", "1"),
        ];
        let out = inject_spans_into_wikitext("foo bar", &toks);
        // foo and bar should be wrapped; baz is silently skipped.
        // bar gets the leading space absorbed.
        assert!(out.wikitext.contains(">foo</span>"));
        assert!(out.wikitext.contains("> bar</span>"));
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;

    fn t(s: &str, editor: &str, class_name: &str) -> WikitextToken {
        WikitextToken {
            str: s.into(),
            editor: editor.into(),
            class_name: class_name.into(),
        }
    }

    /// Repro of the Curzon_Ultimatum bug observed on the first
    /// WMCloud whocolor deploy: spans got injected INSIDE a
    /// `{{Infobox treaty | ... }}` parameter list (template
    /// no_spans=true), breaking MW's parameter parsing. The Infobox
    /// contained a nested `{{bulleted list | [[link]] | [[link]] }}`
    /// — after the inner template's recursion returned, a stray
    /// span was emitted before the Infobox continued.
    #[test]
    fn nested_template_inside_template_does_not_emit_spans() {
        // Simplified Infobox structure mirroring the Curzon case.
        let wikitext = "{{Infobox |a = {{nest |[[L1]] |[[L2]]}}|b = end}}";
        // Tokens (the algorithm tokenizes wikitext including the
        // wiki markers; for this test we just include the words).
        let toks = vec![
            t("infobox", "1", "1"),
            t("a", "1", "1"),
            t("nest", "1", "1"),
            t("l1", "1", "1"),
            t("l2", "1", "1"),
            t("b", "1", "1"),
            t("end", "1", "1"),
        ];
        let out = inject_spans_into_wikitext(wikitext, &toks);
        // No spans should appear anywhere inside the OUTER `{{Infobox ...}}`.
        // The outer template starts at byte 0 and ends at the matching `}}`.
        let infobox_close = out.wikitext.rfind("}}").unwrap() + 2;
        let infobox_body = &out.wikitext[0..infobox_close];
        assert!(
            !infobox_body.contains("<span class=\"editor-token"),
            "no spans should appear inside {{{{Infobox ...}}}}, got: {infobox_body}"
        );
    }
}
