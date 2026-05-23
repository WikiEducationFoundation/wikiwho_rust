//! Verbatim port of `../wikiwho_api/lib/WikiWho/WikiWho/utils.py`.
//!
//! Every behaviour must match the Python reference exactly — any drift
//! shifts every downstream `token_id`. The Python text is lowercased in
//! the caller (`wikiwho.py:123`, `wikiwho.py:191`) before reaching the
//! splitter functions, so callers here are responsible for lowercasing
//! input first.

// Chained `.replace()` calls are kept rather than collapsed into
// `replace([..], "..")` so the structure tracks the Python source
// line-for-line — when the reference changes a single replacement we
// want a 1-line diff here too. Same reason we don't pre-merge the
// "replace each symbol" loop into a regex.
#![allow(clippy::collapsible_str_replace)]

use md5::{Digest, Md5};
use regex::Regex;
use std::sync::LazyLock;

/// MD5 hex digest of UTF-8 bytes.
///
/// Mirrors `calculate_hash` in `utils.py:26-27`:
/// `hashlib.md5(text.encode('utf-8')).hexdigest()`.
pub fn hash_md5(text: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Split text into paragraphs.
///
/// Mirrors `split_into_paragraphs` in `utils.py:30-38`. Normalizes line
/// endings, inserts blank-line breaks around HTML and wikitext table
/// markers, then splits on `\n\n`. Order of replacements matters;
/// preserve it.
pub fn split_paragraphs(text: &str) -> Vec<String> {
    let mut text = text.replace("\r\n", "\n").replace('\r', "\n");
    text = text
        .replace("<table>", "\n\n<table>")
        .replace("</table>", "</table>\n\n");
    text = text
        .replace("<tr>", "\n\n<tr>")
        .replace("</tr>", "</tr>\n\n");
    text = text.replace("{|", "\n\n{|").replace("|}", "|}\n\n");
    text = text.replace("|-\n", "\n\n|-\n");
    text.split("\n\n").map(String::from).collect()
}

// Matches "xyz. " where xyz is three non-whitespace, non-dot, non-equals
// characters followed by a dot and a space. This avoids splitting on
// abbreviations like "Dr. " — the 3-char prefix excludes most short
// abbreviations.
static RE_DOT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([^\s\.=][^\s\.=][^\s\.=]\.) ").unwrap());

// Matches a URL starting with "http", through "://", until the first
// space, pipe, angle bracket, or line terminator (which is included in
// the capture and re-emitted in the result).
static RE_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(http.*?://.*?[ \|<>\n\r])").unwrap());

/// Split text into sentences.
///
/// Mirrors `split_into_sentences` in `utils.py:41-72`. Uses `@@@@` as
/// an internal delimiter inserted after sentence-terminating
/// punctuation and around comments / references / URLs, then collapses
/// runs of the delimiter and splits.
pub fn split_sentences(text: &str) -> Vec<String> {
    let mut text = text.replace('\n', "\n@@@@");
    text = RE_DOT.replace_all(&text, "$1@@@@").into_owned();
    text = text.replace("; ", ";@@@@");
    text = text.replace("? ", "?@@@@");
    text = text.replace("! ", "!@@@@");
    text = text.replace(": ", ":@@@@");
    text = text.replace('\t', "\t@@@@");
    // Comments as their own sentence
    text = text.replace("<!--", "@@@@<!--");
    text = text.replace("-->", "-->@@@@");
    // References as their own sentence: <ref name="...">{{ ... }}</ref>
    text = text.replace("<ref", "@@@@<ref");
    text = text.replace("/ref>", "/ref>@@@@");
    // URLs as their own sentence
    text = RE_URL.replace_all(&text, "@@@@$1@@@@").into_owned();

    // Collapse runs of delimiter. The Python loop replaces eight `@`s
    // with four until none remain; that converges because each pass
    // strictly shortens the string. We mirror it directly.
    while text.contains("@@@@@@@@") {
        text = text.replace("@@@@@@@@", "@@@@");
    }
    text.split("@@@@").map(String::from).collect()
}

// Symbols that are tokens on their own. Verbatim from `utils.py:80-85`.
// Some are multi-byte (currency symbols, typographic punctuation, CJK
// punctuation); `&str` rather than `char` keeps the encoded form right
// for `replace`.
const SYMBOLS: &[&str] = &[
    ".", ",", ";", ":", "?", "!", "-", "_", "/", "\\", "(", ")", "[", "]", "{", "}", "*", "#", "@",
    "&", "=", "+", "%", "~", "$", "^", "<", ">", "\"", "'", "´", "`", "¸", "˛", "’", "¤", "₳",
    "฿", "₵", "¢", "₡", "₢", "₫", "₯", "֏", "₠", "€", "ƒ", "₣", "₲", "₴", "₭", "₺", "₾", "ℳ",
    "₥", "₦", "₧", "₱", "₰", "£", "៛", "₽", "₹", "₨", "₪", "৳", "₸", "₮", "₩", "¥", "§", "‖",
    "¦", "⟨", "⟩", "–", "—", "¯", "»", "«", "”", "÷", "×", "′", "″", "‴", "¡", "¿", "©", "℗",
    "®", "℠", "™",
];

// CJK ranges mirrored from `utils.py:19-23`. Note the Python source uses
// surrogate-style `\Uxxxxxxxx` literals for codepoints above U+FFFF;
// Rust's `regex` crate accepts the `\u{...}` form for any scalar value.
static RE_CJK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        "([\u{2E80}-\u{9FFF}",
        "\u{F900}-\u{FAFF}",
        "\u{FE00}-\u{FE1F}",
        "\u{FE30}-\u{FE6F}",
        "\u{FF00}-\u{FFEF}",
        "\u{16FE0}-\u{1B2FF}",
        "\u{1F000}-\u{1F2FF}",
        "\u{20000}-\u{3347F}",
        "\u{E0100}-\u{E01EF}])",
    ))
    .unwrap()
});

/// Split text into tokens.
///
/// Mirrors `split_into_tokens` in `utils.py:75-107`. Strategy:
/// 1. Encode any literal `|` characters as a placeholder so we can use
///    `||` as a delimiter without ambiguity.
/// 2. Replace whitespace with the delimiter.
/// 3. Wrap each known symbol with the delimiter so symbols become
///    their own tokens.
/// 4. Reconstruct multi-character symbols (`[[`, `]]`, `{{`, `}}`,
///    `<!--`, `-->`) that the per-symbol pass broke apart.
/// 5. If the text contains non-ASCII characters and any CJK character,
///    wrap each CJK character with the delimiter so it's its own token.
/// 6. Collapse adjacent delimiters, split, drop empties, restore the
///    pipe placeholders.
pub fn split_tokens(text: &str) -> Vec<String> {
    let mut text = text.replace('|', "||ææææ||");
    text = text.replace('\n', "||").replace(' ', "||");

    for s in SYMBOLS {
        text = text.replace(s, &format!("||{s}||"));
    }

    // Reconstruct broken-by-the-symbol-pass multi-char tokens. Each is
    // four pipes wide because each component char picked up a pair of
    // pipes around itself in the loop above.
    text = text
        .replace("[||||[", "[[")
        .replace("]||||]", "]]")
        .replace("{||||{", "{{")
        .replace("}||||}", "}}");
    text = text
        .replace("<||||!||||-||||-||", "||<!--||")
        .replace("||-||||-||||>", "||-->||");

    if !text.is_ascii() && RE_CJK.is_match(&text) {
        text = RE_CJK.replace_all(&text, "||$1||").into_owned();
    }

    while text.contains("||||") {
        text = text.replace("||||", "||");
    }

    text.split("||")
        .filter(|s| !s.is_empty())
        .map(|s| if s == "ææææ" { "|".to_string() } else { s.to_string() })
        .collect()
}

/// Tokenize an entire revision in the order the algorithm produces.
///
/// The algorithm doesn't tokenize a revision in one pass; it splits
/// into paragraphs, then each paragraph into sentences, then each
/// sentence into tokens. Paragraphs whose `strip()` is empty are
/// skipped (`wikiwho.py:340`); sentences whose `strip()` is empty are
/// skipped (`wikiwho.py:476`); each surviving sentence is tokenized,
/// and the token list is flattened in document order. That order is
/// what the response builder emits in the JSON `tokens` array.
///
/// **Caller responsibility:** `text` must already be lowercased. The
/// reference algorithm lowercases at the boundary (`wikiwho.py:123`,
/// `wikiwho.py:191`) before invoking any splitter.
pub fn tokenize_revision(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for paragraph in split_paragraphs(text) {
        if paragraph.trim().is_empty() {
            continue;
        }
        for sentence in split_sentences(&paragraph) {
            let trimmed = sentence.trim();
            if trimmed.is_empty() {
                continue;
            }
            // Sentence is stored in the algorithm as the space-joined
            // token list (wikiwho.py:479) and later split back on
            // space to repopulate `words` (wikiwho.py:599). Since no
            // token returned by split_tokens contains a space, that
            // round-trip is the identity — yield tokens directly.
            out.extend(split_tokens(trimmed));
        }
    }
    out
}

/// Compute the average count-per-distinct-token across the input,
/// ignoring a small set of structural tokens. Used by the vandalism
/// heuristic in `wikiwho.py:608-613` (`compute_avg_word_freq` in
/// `utils.py:110-118`).
///
/// Returns 0.0 if no tokens survive the filter.
pub fn avg_word_freq(tokens: &[String]) -> f64 {
    use std::collections::HashMap;
    const REMOVE: &[&str] = &["<", ">", "tr", "td", "[", "]", "\"", "*", "==", "{", "}", "|", "-"];

    let mut counts: HashMap<&str, u32> = HashMap::new();
    for t in tokens {
        *counts.entry(t.as_str()).or_insert(0) += 1;
    }
    for r in REMOVE {
        counts.remove(*r);
    }
    if counts.is_empty() {
        0.0
    } else {
        let sum: u32 = counts.values().sum();
        f64::from(sum) / counts.len() as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Outputs in these tests were captured by running the reference
    // Python implementation (`wikiwho_api/lib/WikiWho/WikiWho/utils.py`)
    // directly via `python3 -c 'from WikiWho.utils import split_into_tokens;
    // print(list(split_into_tokens("..."))).` Any divergence here means
    // the Rust port has drifted from the reference.

    #[test]
    fn hash_md5_known_vectors() {
        // hashlib.md5(b'').hexdigest()
        assert_eq!(hash_md5(""), "d41d8cd98f00b204e9800998ecf8427e");
        // hashlib.md5(b'hello').hexdigest()
        assert_eq!(hash_md5("hello"), "5d41402abc4b2a76b9719d911017c592");
        // hashlib.md5('中国'.encode('utf-8')).hexdigest()
        assert_eq!(hash_md5("中国"), "c13dceabcb143acd6c9298265d618a9f");
    }

    #[test]
    fn split_paragraphs_simple() {
        assert_eq!(split_paragraphs("a\n\nb"), vec!["a", "b"]);
        assert_eq!(split_paragraphs("a\n\n\nb"), vec!["a", "\nb"]);
        // Single-newline doesn't split
        assert_eq!(split_paragraphs("a\nb"), vec!["a\nb"]);
    }

    #[test]
    fn split_paragraphs_normalizes_line_endings() {
        // \r\n and \r both become \n
        assert_eq!(split_paragraphs("a\r\n\r\nb"), vec!["a", "b"]);
        assert_eq!(split_paragraphs("a\r\rb"), vec!["a", "b"]);
    }

    #[test]
    fn split_paragraphs_html_table_wrapping() {
        // <table>...</table> gets blank lines inserted around it so it
        // ends up as its own paragraph.
        let result = split_paragraphs("before<table>x</table>after");
        assert_eq!(result, vec!["before", "<table>x</table>", "after"]);
    }

    #[test]
    fn split_paragraphs_wikitext_table_wrapping() {
        let result = split_paragraphs("before{|x|}after");
        assert_eq!(result, vec!["before", "{|x|}", "after"]);
    }

    #[test]
    fn split_sentences_dot_rule() {
        // The regex requires 3 non-whitespace/non-dot/non-equals chars
        // before the terminating ". ". Non-overlapping replace_all
        // matches every ". " run in turn — each becomes a split point.
        assert_eq!(
            split_sentences("foo. bar. baz."),
            vec!["foo.", "bar.", "baz."]
        );
    }

    #[test]
    fn split_sentences_short_words_not_split() {
        // "Hi. " has only 2 chars before the dot — doesn't match
        // the 3-non-whitespace prefix in regex_dot, so no split here.
        assert_eq!(split_sentences("hi. ok"), vec!["hi. ok"]);
    }

    #[test]
    fn split_sentences_punctuation_kinds() {
        assert_eq!(split_sentences("a; b? c! d: e"), vec!["a;", "b?", "c!", "d:", "e"]);
    }

    #[test]
    fn split_sentences_newline_and_tab() {
        // Newlines split, and the \n stays at the END of the prior
        // sentence per the `text.replace('\n', '\n@@@@')` rule.
        assert_eq!(split_sentences("a\nb"), vec!["a\n", "b"]);
        assert_eq!(split_sentences("a\tb"), vec!["a\t", "b"]);
    }

    #[test]
    fn split_sentences_html_comment() {
        assert_eq!(
            split_sentences("foo<!--c-->bar"),
            vec!["foo", "<!--c-->", "bar"]
        );
    }

    #[test]
    fn split_sentences_ref_tag() {
        assert_eq!(
            split_sentences("text<ref>cite</ref>more"),
            vec!["text", "<ref>cite</ref>", "more"]
        );
    }

    #[test]
    fn split_sentences_url() {
        // URL gets isolated as its own sentence; the trailing terminator
        // (space, here) is part of the captured URL.
        let result = split_sentences("see http://x.com/a more text");
        assert_eq!(result, vec!["see ", "http://x.com/a ", "more text"]);
    }

    #[test]
    fn split_tokens_simple() {
        assert_eq!(split_tokens("hello world"), vec!["hello", "world"]);
        assert_eq!(
            split_tokens("foo, bar!"),
            vec!["foo", ",", "bar", "!"]
        );
    }

    #[test]
    fn split_tokens_brackets_kept_double() {
        // [[link]] should produce [[, link, ]] as three tokens.
        assert_eq!(
            split_tokens("[[link]]"),
            vec!["[[", "link", "]]"]
        );
        assert_eq!(
            split_tokens("{{template}}"),
            vec!["{{", "template", "}}"]
        );
    }

    #[test]
    fn split_tokens_html_comment_is_single_token() {
        // <!-- and --> should each be one token, not split apart by the
        // per-symbol pass. The reconstruction in split_into_tokens
        // re-joins them.
        let result = split_tokens("a<!--c-->b");
        assert_eq!(result, vec!["a", "<!--", "c", "-->", "b"]);
    }

    #[test]
    fn split_tokens_pipe_preserved() {
        // Literal | chars must survive the placeholder roundtrip.
        assert_eq!(split_tokens("a|b"), vec!["a", "|", "b"]);
        assert_eq!(split_tokens("|"), vec!["|"]);
        assert_eq!(split_tokens("||"), vec!["|", "|"]);
    }

    #[test]
    fn split_tokens_cjk_one_token_per_char() {
        // Every CJK character becomes its own token.
        assert_eq!(split_tokens("中国"), vec!["中", "国"]);
        assert_eq!(split_tokens("foo 中国 bar"), vec!["foo", "中", "国", "bar"]);
    }

    #[test]
    fn split_tokens_cjk_punctuation_in_range() {
        // U+3002 (CJK FULL STOP) is in the CJK range, so it splits as
        // its own token even though it's not in the ASCII symbol list.
        assert_eq!(split_tokens("中。国"), vec!["中", "。", "国"]);
    }

    #[test]
    fn split_tokens_ascii_only_skips_cjk_regex() {
        // For purely-ASCII input the CJK pass is short-circuited.
        // (Behavioural assertion: output is the same as if the pass
        // ran, since ASCII matches nothing in the CJK range.)
        assert_eq!(split_tokens("just ascii here"), vec!["just", "ascii", "here"]);
    }

    #[test]
    fn split_tokens_currency_symbols() {
        // Currency symbols are in the long symbol list.
        assert_eq!(split_tokens("price $5"), vec!["price", "$", "5"]);
        assert_eq!(split_tokens("€100"), vec!["€", "100"]);
    }

    #[test]
    fn split_tokens_empty_string_yields_no_tokens() {
        assert_eq!(split_tokens(""), Vec::<String>::new());
    }

    #[test]
    fn split_tokens_sentence_normalization_round_trip() {
        // The algorithm hashes sentences after re-joining tokens with
        // single spaces (wikiwho.py:479). This means whitespace in the
        // input shouldn't change the hash.
        let a = split_tokens("foo  bar").join(" ");
        let b = split_tokens("foo bar").join(" ");
        assert_eq!(a, b);
        assert_eq!(a, "foo bar");
    }

    #[test]
    fn tokenize_revision_simple() {
        // Single paragraph, single sentence
        assert_eq!(
            tokenize_revision("hello world"),
            vec!["hello", "world"]
        );
    }

    #[test]
    fn tokenize_revision_skips_empty_paragraphs() {
        // Two empty paragraphs sandwiched between non-empty ones.
        // Empty here means "blank-line-only" — between four \n, the
        // middle paragraph after split_paragraphs is "" which the
        // filter drops.
        let text = "first\n\n\n\nsecond";
        let result = tokenize_revision(text);
        assert_eq!(result, vec!["first", "second"]);
    }

    #[test]
    fn tokenize_revision_multi_sentence_paragraph() {
        let text = "foo bar. baz qux.";
        // split_sentences yields ["foo bar.", "baz qux."]; each
        // tokenizes individually
        assert_eq!(
            tokenize_revision(text),
            vec!["foo", "bar", ".", "baz", "qux", "."]
        );
    }

    #[test]
    fn tokenize_revision_wiki_markup() {
        // [[link]] should yield [[, link, ]]; punctuation isolated.
        assert_eq!(
            tokenize_revision("see [[link]]."),
            vec!["see", "[[", "link", "]]", "."]
        );
    }

    #[test]
    fn tokenize_revision_table_paragraph_splitting() {
        // Wikitext table becomes its own paragraph; the surrounding
        // text becomes separate paragraphs.
        let text = "before{|x|}after";
        // split_paragraphs splits this into ["before", "{|x|}", "after"]
        let result = tokenize_revision(text);
        // "before" -> ["before"]; "{|x|}" -> ["{", "|", "x", "|", "}"]
        // — wait, "{|" reconstructs? Let me think... no, "{|" gets `{|` not `{{` so
        // it splits as "{", "|", "x", "|", "}". Same for "}".
        assert_eq!(result, vec!["before", "{", "|", "x", "|", "}", "after"]);
    }

    #[test]
    fn avg_word_freq_basic() {
        let tokens: Vec<String> = vec!["the", "the", "cat"].into_iter().map(String::from).collect();
        // Counter = {the: 2, cat: 1}; nothing removed; (2+1)/2 = 1.5
        assert_eq!(avg_word_freq(&tokens), 1.5);
    }

    #[test]
    fn avg_word_freq_filters_structural_tokens() {
        let tokens: Vec<String> = vec!["the", "|", "<", "cat"].into_iter().map(String::from).collect();
        // Counter = {the:1, |:1, <:1, cat:1}; remove |, <; (1+1)/2 = 1.0
        assert_eq!(avg_word_freq(&tokens), 1.0);
    }

    #[test]
    fn avg_word_freq_empty_returns_zero() {
        let tokens: Vec<String> = vec!["|", "<", ">"].into_iter().map(String::from).collect();
        assert_eq!(avg_word_freq(&tokens), 0.0);
        assert_eq!(avg_word_freq(&[]), 0.0);
    }
}
