//! Vandalism detection.
//!
//! Port of the spam-related logic in
//! `../wikiwho_api/lib/WikiWho/WikiWho/wikiwho.py:22-29` (constants),
//! `:80-92` (length-based heuristic + hash check on the XML path),
//! and `:156-168` (the JSON path). The full semantics are spelled out
//! in `../../ALGORITHM.md §3`.
//!
//! There are three independent checks. A revision is vandalism if
//! ANY of them fires:
//!
//! 1. **Hash match.** If the revision's SHA-1 matches a hash we've
//!    already classified as spam (`hash_matches_known_spam`), it's
//!    automatically spam. Filters cheap repeated-vandalism quickly.
//! 2. **Length shrink** (this module's `length_shrink_is_vandalism`).
//!    If the previous revision was sizable AND this one is small
//!    AND the relative drop exceeds 40%, it's vandalism — unless the
//!    edit was marked as a good-faith move (`comment AND minor`).
//! 3. **Token-density check** in the matching cascade
//!    (`analyse_words_in_sentences`). Lives in the cascade module
//!    once that's written; the constants it consumes
//!    (`TOKEN_DENSITY_LIMIT`, `TOKEN_LEN`) are exported here for
//!    cross-module use.

#![allow(dead_code)]

/// Threshold for the length-shrink heuristic. A negative ratio because
/// shrinkage is measured as `(curr - prev) / prev`. Verbatim from
/// `wikiwho.py:23`.
pub const CHANGE_PERCENTAGE: f64 = -0.40;

/// Minimum size (chars) of the previous revision for the length-shrink
/// heuristic to apply. Tiny articles aren't checked because a small
/// absolute change can swamp the relative ratio.
pub const PREVIOUS_LENGTH: usize = 1000;

/// Maximum size (chars) of the current revision for the length-shrink
/// heuristic to apply. Together with PREVIOUS_LENGTH this gates the
/// check to "big article suddenly tiny."
pub const CURR_LENGTH: usize = 1000;

/// Flag word used by editors when moving content between articles
/// (e.g., split / merge edits). Currently unused at the API boundary
/// — the heuristic instead checks `comment AND minor` to detect
/// good-faith bulk operations.
pub const MOVE_FLAG: &str = "move";

/// Ratio of unmatched paragraphs above which the third-tier
/// vandalism check (token-density) fires. `0.0` means "any unmatched
/// paragraph triggers possible_vandalism." See `ALGORITHM.md §4` for
/// the cascade interaction.
pub const UNMATCHED_PARAGRAPH: f64 = 0.0;

/// Threshold for the token-density confirmation step. Computed by
/// `tokenize::avg_word_freq` over the current revision's flattened
/// token sequence; if the average count-per-distinct-token exceeds
/// this, the cascade calls it vandalism. Catches copy-paste vandalism
/// (high token repetition).
pub const TOKEN_DENSITY_LIMIT: f64 = 20.0;

/// Tokens shorter than this length are ignored when computing average
/// token frequency. Verbatim from `wikiwho.py:29` — 100 chars; in
/// practice almost no real token is that long, so this exclusion
/// rarely fires. Preserved for parity.
pub const TOKEN_LEN: usize = 100;

/// True if the revision's hash matches a previously-flagged spam hash.
///
/// Mirrors the membership check at `wikiwho.py:80-81` / `:156-157`:
/// `if rev_hash in self.spam_hashes: vandalism = True`.
pub fn hash_matches_known_spam(rev_hash: &str, spam_hashes: &std::collections::HashSet<String>) -> bool {
    spam_hashes.contains(rev_hash)
}

/// True if the length-shrink heuristic flags this revision as
/// vandalism. Verbatim from `wikiwho.py:85-92` / `:161-168`:
///
/// ```text
/// if not (comment AND minor)
///    and prev_length > PREVIOUS_LENGTH
///    and curr_length < CURR_LENGTH
///    and (curr_length - prev_length) / prev_length <= CHANGE_PERCENTAGE:
///     vandalism = True
/// ```
///
/// `comment` is whether the revision has a non-empty edit summary;
/// `minor` is whether the editor marked it as a minor edit. Together
/// they mark a "good-faith bulk move" that should NOT be flagged even
/// if it shrinks the article.
///
/// `prev_length == 0` is treated as "no previous revision" and the
/// heuristic does not fire (avoids div-by-zero; matches Python
/// because `> 1000` is false at 0).
pub fn length_shrink_is_vandalism(
    prev_length: usize,
    curr_length: usize,
    comment: bool,
    minor: bool,
) -> bool {
    // Good-faith move escape hatch.
    if comment && minor {
        return false;
    }
    // The two size thresholds gate the check to "big became small".
    if prev_length <= PREVIOUS_LENGTH {
        return false;
    }
    if curr_length >= CURR_LENGTH {
        return false;
    }
    // Relative shrink, signed. prev_length > 0 is guaranteed by the
    // gate above.
    let ratio = (curr_length as f64 - prev_length as f64) / prev_length as f64;
    ratio <= CHANGE_PERCENTAGE
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn hash_match_membership() {
        let mut spam: HashSet<String> = HashSet::new();
        spam.insert("abc123".into());
        assert!(hash_matches_known_spam("abc123", &spam));
        assert!(!hash_matches_known_spam("def456", &spam));
    }

    #[test]
    fn length_shrink_obama_to_nothing_is_vandalism() {
        // 500 KB -> 100 bytes, unflagged edit. Classic blanking
        // vandalism on a big article.
        assert!(length_shrink_is_vandalism(500_000, 100, false, false));
    }

    #[test]
    fn length_shrink_good_faith_move_is_not_vandalism() {
        // Same blanking, but the editor marked it minor with a
        // comment — could be a "moved content to X" edit. Escape.
        assert!(!length_shrink_is_vandalism(500_000, 100, true, true));
    }

    #[test]
    fn length_shrink_below_previous_threshold_does_not_fire() {
        // Previous revision was too small for the heuristic to apply.
        // (E.g., a stub got blanked. Don't auto-flag.)
        assert!(!length_shrink_is_vandalism(500, 50, false, false));
        // Equal-to-threshold also doesn't fire — Python uses strict `>`.
        assert!(!length_shrink_is_vandalism(1000, 50, false, false));
    }

    #[test]
    fn length_shrink_curr_above_threshold_does_not_fire() {
        // Article was big, became smaller but still substantial.
        // Don't flag — could be legitimate content removal.
        assert!(!length_shrink_is_vandalism(10_000, 5_000, false, false));
        assert!(!length_shrink_is_vandalism(10_000, 1000, false, false));
    }

    #[test]
    fn length_shrink_ratio_at_threshold() {
        // exactly 40% shrink — qualifies (Python uses `<=`).
        // Need curr < CURR_LENGTH (1000) AND prev > PREVIOUS_LENGTH
        // (1000), so prev = 1500, curr = 900 sits exactly at -0.40.
        assert!(length_shrink_is_vandalism(1500, 900, false, false));
    }

    #[test]
    fn length_shrink_modest_shrink_does_not_fire() {
        // ~37% shrink — under the -0.40 threshold. prev=1500 so the
        // size gates are satisfied; ratio is what blocks the flag.
        assert!(!length_shrink_is_vandalism(1500, 950, false, false));
    }

    #[test]
    fn length_shrink_growth_does_not_fire() {
        // Article grew. Ratio is positive, well above the threshold.
        assert!(!length_shrink_is_vandalism(10_000, 12_000, false, false));
    }

    #[test]
    fn length_shrink_zero_prev_does_not_panic() {
        // First-revision edge case: no previous content.
        assert!(!length_shrink_is_vandalism(0, 100, false, false));
        assert!(!length_shrink_is_vandalism(0, 0, false, false));
    }

    #[test]
    fn comment_alone_does_not_escape() {
        // Just a comment, not minor. Heuristic still applies.
        assert!(length_shrink_is_vandalism(10_000, 100, true, false));
    }

    #[test]
    fn minor_alone_does_not_escape() {
        // Just minor, no comment. Heuristic still applies.
        assert!(length_shrink_is_vandalism(10_000, 100, false, true));
    }
}
