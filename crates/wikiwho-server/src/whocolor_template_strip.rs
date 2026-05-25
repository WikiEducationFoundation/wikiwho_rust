//! POC: template-strip optimization for the whocolor first-call path.
//!
//! ## Why
//!
//! The current `whocolor_wikitext` flow injects span markers into the
//! article wikitext and POSTs it to MW's action=parse. Because we
//! modify the wikitext, MW's parser cache always misses, so every call
//! pays the full template-expansion + Lua cost — ~12s for a large
//! article like Photosynthesis.
//!
//! Tokens inside `{{...}}` templates are already suppressed from
//! span-wrapping (see `whocolor_wikitext::SPECIAL_MARKUPS` —
//! `no_spans: true` on the Template entry). The expanded template HTML
//! contributes no span markup, only structural content. That makes
//! templates an obvious thing to lift OUT of the modified-wikitext
//! parse: render them separately (or pull from MW's cache for the
//! unmodified article), and let the modified-wikitext parse only deal
//! with the cheap remainder.
//!
//! ## Flow
//!
//! 1. Find top-level `{{…}}` ranges in the wikitext.
//! 2. Replace each with a unique PUA-bounded placeholder
//!    `\u{E000}T<n>\u{E001}`. MW's parser passes PUA codepoints
//!    through unmodified, so the placeholder appears verbatim in the
//!    rendered output.
//! 3. Inject spans into the stripped wikitext. Tokens whose strings
//!    only existed inside templates won't be findable in the stripped
//!    wikitext and are silently skipped by `set_token` — that matches
//!    the existing no-spans-inside-templates behavior.
//! 4. In parallel: POST the stripped+spans wikitext, AND POST each
//!    extracted template's wikitext individually. The main POST is
//!    fast (no templates to expand); each template POST is fast (small
//!    input, MW may cache repeated invocations of common templates).
//! 5. Splice each rendered template back into the stripped HTML at
//!    its placeholder position.
//!
//! ## Trade-offs / caveats
//!
//! - **Templates that affect surrounding parsing** (e.g. `{{!}}` which
//!   expands to `|` and is used inside table syntax) will render
//!   differently when rendered out-of-context. The POC accepts this;
//!   articles where it matters need the full-parse fallback.
//! - **No `<nowiki>` / comment skipping** in the template-range
//!   finder. False-positive `{{...}}` matches inside nowiki/comments
//!   would mis-strip. Rare in practice.
//! - **N parallel MW calls** (one per template). On rate-limit budget
//!   exhaustion the caller should fall back; this module just
//!   propagates the error.

use wikiwho_mwclient::{MwClient, MwError};

use crate::whocolor_html::PresentEditorEntry;
use crate::whocolor_wikitext::{inject_spans_into_wikitext, WikitextToken};

const PLACEHOLDER_OPEN: char = '\u{E000}';
const PLACEHOLDER_CLOSE: char = '\u{E001}';

/// Top-level `{{...}}` byte ranges in document order. POC scope:
/// counts `{{` / `}}` pairs with a depth counter; does not skip
/// matches inside `<nowiki>` or `<!-- -->`.
pub fn find_top_level_template_ranges(wikitext: &str) -> Vec<(usize, usize)> {
    let bytes = wikitext.as_bytes();
    let mut ranges = Vec::new();
    let n = bytes.len();
    let mut i = 0;
    let mut depth = 0u32;
    let mut current_start: Option<usize> = None;
    while i + 1 < n {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            if depth == 0 {
                current_start = Some(i);
            }
            depth = depth.saturating_add(1);
            i += 2;
            continue;
        }
        if bytes[i] == b'}' && bytes[i + 1] == b'}' {
            if depth > 0 {
                depth -= 1;
                if depth == 0 {
                    if let Some(start) = current_start.take() {
                        ranges.push((start, i + 2));
                    }
                }
            }
            i += 2;
            continue;
        }
        i += 1;
    }
    ranges
}

/// Replace each (start, end) byte range with a placeholder
/// `\u{E000}T<index>\u{E001}`. Returns the stripped wikitext and the
/// extracted template texts (parallel-indexed with the placeholders).
pub fn strip_templates(wikitext: &str, ranges: &[(usize, usize)]) -> (String, Vec<String>) {
    let mut stripped = String::with_capacity(wikitext.len());
    let mut templates = Vec::with_capacity(ranges.len());
    let mut cursor = 0;
    for (i, &(start, end)) in ranges.iter().enumerate() {
        stripped.push_str(&wikitext[cursor..start]);
        stripped.push(PLACEHOLDER_OPEN);
        stripped.push_str(&format!("T{i}"));
        stripped.push(PLACEHOLDER_CLOSE);
        templates.push(wikitext[start..end].to_string());
        cursor = end;
    }
    stripped.push_str(&wikitext[cursor..]);
    (stripped, templates)
}

/// Replace each `\u{E000}T<n>\u{E001}` marker in `stripped_html` with
/// `rendered_templates[n]`. Malformed or out-of-range markers are
/// passed through verbatim.
pub fn splice_templates(stripped_html: &str, rendered_templates: &[String]) -> String {
    let open_len = PLACEHOLDER_OPEN.len_utf8();
    let close_len = PLACEHOLDER_CLOSE.len_utf8();
    let total_cap =
        stripped_html.len() + rendered_templates.iter().map(|t| t.len()).sum::<usize>();
    let mut result = String::with_capacity(total_cap);
    let mut cursor = 0;
    while let Some(rel_open) = stripped_html[cursor..].find(PLACEHOLDER_OPEN) {
        let abs_open = cursor + rel_open;
        let after_open = abs_open + open_len;
        let close_rel = match stripped_html[after_open..].find(PLACEHOLDER_CLOSE) {
            Some(o) => o,
            None => break,
        };
        let abs_close = after_open + close_rel;
        let id_str = &stripped_html[after_open..abs_close];
        let after_close = abs_close + close_len;
        result.push_str(&stripped_html[cursor..abs_open]);
        let mut spliced = false;
        if let Some(idx_str) = id_str.strip_prefix('T') {
            if let Ok(idx) = idx_str.parse::<usize>() {
                if let Some(tpl) = rendered_templates.get(idx) {
                    result.push_str(tpl);
                    spliced = true;
                }
            }
        }
        if !spliced {
            result.push_str(&stripped_html[abs_open..after_close]);
        }
        cursor = after_close;
    }
    result.push_str(&stripped_html[cursor..]);
    result
}

/// Build the whocolor `extended_html` via the template-strip flow.
///
/// On any MW error the caller should fall back to the full-parse path.
pub async fn build_extended_html(
    mw: &MwClient,
    title: &str,
    wikitext: &str,
    tokens: &[WikitextToken],
) -> Result<(String, Vec<PresentEditorEntry>), MwError> {
    let ranges = find_top_level_template_ranges(wikitext);
    let (stripped_wikitext, templates) = strip_templates(wikitext, &ranges);

    tracing::info!(
        title = %title,
        n_templates = templates.len(),
        wikitext_bytes = wikitext.len(),
        stripped_bytes = stripped_wikitext.len(),
        "template-strip whocolor",
    );

    // CPU-bound — bounce off the blocking pool. Tokens whose strings
    // only existed inside templates won't be findable in
    // `stripped_wikitext` and are silently skipped by `set_token`.
    let stripped_for_task = stripped_wikitext.clone();
    let tokens_for_task: Vec<WikitextToken> = tokens.to_vec();
    let injection = tokio::task::spawn_blocking(move || {
        inject_spans_into_wikitext(&stripped_for_task, &tokens_for_task)
    })
    .await
    .map_err(|e| {
        MwError::Shape(format!("strip-templates injection task panicked: {e}"))
    })?;

    // Parallel: main parse + per-template parses. Same `title` for
    // every call so context-sensitive magic words ({{PAGENAME}} etc.)
    // resolve the same way.
    let main_parse = mw.parse_wikitext(title, &injection.wikitext);
    let mut template_handles = Vec::with_capacity(templates.len());
    for tpl in &templates {
        let mw_clone = mw.clone();
        let title_clone = title.to_string();
        let tpl_clone = tpl.clone();
        template_handles.push(tokio::spawn(async move {
            mw_clone.parse_wikitext(&title_clone, &tpl_clone).await
        }));
    }

    let main_html = main_parse.await?;
    let mut rendered_templates: Vec<String> = Vec::with_capacity(template_handles.len());
    for h in template_handles {
        let res = h.await.map_err(|e| {
            MwError::Shape(format!("strip-templates template parse task panicked: {e}"))
        })?;
        rendered_templates.push(res?);
    }

    let final_html = splice_templates(&main_html, &rendered_templates);

    let present_editors = injection
        .present_editors
        .into_iter()
        .map(|e| PresentEditorEntry {
            editor: e.editor,
            class_name: e.class_name,
            token_count: e.token_count,
        })
        .collect();

    Ok((final_html, present_editors))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_top_level_ranges_and_skips_nested() {
        let wt = "before {{outer|{{inner|x}}}} middle {{simple}} end";
        let ranges = find_top_level_template_ranges(wt);
        // Two top-level templates: outer (contains inner) and simple.
        assert_eq!(ranges.len(), 2);
        assert_eq!(&wt[ranges[0].0..ranges[0].1], "{{outer|{{inner|x}}}}");
        assert_eq!(&wt[ranges[1].0..ranges[1].1], "{{simple}}");
    }

    #[test]
    fn strip_then_splice_roundtrip() {
        let wt = "Hello {{a|1}} world {{b|2}}!";
        let ranges = find_top_level_template_ranges(wt);
        let (stripped, templates) = strip_templates(wt, &ranges);
        assert_eq!(templates, vec!["{{a|1}}".to_string(), "{{b|2}}".to_string()]);
        assert!(stripped.contains('\u{E000}'));
        // Pretend the parser returned the stripped wikitext verbatim,
        // and that each template "renders" to itself surrounded by
        // <i>…</i>. Splicing should produce the expected output.
        let rendered: Vec<String> = templates
            .iter()
            .map(|t| format!("<i>{t}</i>"))
            .collect();
        let final_html = splice_templates(&stripped, &rendered);
        assert_eq!(final_html, "Hello <i>{{a|1}}</i> world <i>{{b|2}}</i>!");
    }

    #[test]
    fn malformed_placeholder_passes_through() {
        let weird = format!("x{PLACEHOLDER_OPEN}T999{PLACEHOLDER_CLOSE}y");
        // Only 1 template rendered, so T999 is out of range.
        let rendered = vec!["RENDERED".to_string()];
        let out = splice_templates(&weird, &rendered);
        // The placeholder should be preserved verbatim, NOT replaced.
        assert!(out.contains("T999"));
    }
}
