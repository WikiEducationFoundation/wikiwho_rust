//! Port of Python's `difflib.SequenceMatcher` + `difflib.Differ`.
//!
//! Replaces the Myers-based diff in `diff.rs` inside the token cascade.
//! See `notes/2026-05-23-python-replay.md` and `ALGORITHM.md §6` for
//! the rationale: Python's `Differ` uses Ratcliff/Obershelp pattern
//! matching (find longest contiguous block, recurse on the halves) and
//! produces a *different* set of matches than true-LCS Myers — the
//! cascade observes ~3–4% more "Keep" ops under Myers, which cascades
//! into ~14% per-token `o_rev_id` divergence on Photosynthesis. Three
//! of four production consumers (Dashboard, XTools, WhoWroteThat)
//! render attribution per-token, so we close that gap by matching
//! Python exactly. Myers is left in place in `diff.rs` for a future
//! re-evaluation — see `notes/diff-algorithm-revisit.md`.
//!
//! The port mirrors `difflib.py` line-for-line where it matters for
//! cascade output: `find_longest_match`, `get_matching_blocks`,
//! `get_opcodes`, plus the `Differ.compare`/`_fancy_replace`/
//! `_plain_replace` chain. Autojunk (popular-element elision for
//! sequences ≥ 200 elements) is included; explicit `isjunk` is not
//! (`Differ()` and the inner `SequenceMatcher` are both constructed
//! with no junk callback in `wikiwho.py:631`). The `'? '` hint lines
//! that `Differ` emits for human-readable diffs are dropped — the
//! cascade filters them out anyway.
//!
//! Reference: `/usr/lib/python3.13/difflib.py` lines 44–663
//! (SequenceMatcher) and 724–1024 (Differ).

use std::collections::HashMap;
use std::hash::Hash;

/// The `(prev, curr)` token-level edit transcript Differ produces. The
/// payload is the token string (cloned out of the input slices) so
/// callers can match by value the same way the Python cascade does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffOp {
    /// `'  ' + token` — token kept across the revision. Carries the
    /// `text_prev` value (which equals `text_curr` for `Equal` blocks).
    Keep(String),
    /// `'- ' + token` — token present in `text_prev` only.
    Delete(String),
    /// `'+ ' + token` — token present in `text_curr` only.
    Insert(String),
}

/// One matching block in the Ratcliff/Obershelp decomposition:
/// `a[a_start..a_start+size] == b[b_start..b_start+size]`. Mirrors
/// Python's `difflib.Match` namedtuple.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Match {
    pub a_start: usize,
    pub b_start: usize,
    pub size: usize,
}

/// Opcode tag for a single segment of the diff. The names match
/// Python's string tags exactly (`'equal'`, `'delete'`, `'insert'`,
/// `'replace'`); we use an enum for type safety.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpTag {
    Equal,
    Delete,
    Insert,
    Replace,
}

/// One opcode segment: a tag plus the half-open ranges `a[i1..i2]` and
/// `b[j1..j2]`. Mirrors the `(tag, i1, i2, j1, j2)` tuples Python
/// returns from `SequenceMatcher.get_opcodes()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpCode {
    pub tag: OpTag,
    pub i1: usize,
    pub i2: usize,
    pub j1: usize,
    pub j2: usize,
}

/// Ratcliff/Obershelp matcher. Owns clones of its two input sequences
/// so that `set_seq1`/`set_seq2` can be called repeatedly (mirroring
/// the `_fancy_replace` inner loop, which creates one matcher and
/// resets seq2/seq1 per pair). `T` is the element type — `u32` for
/// the outer token-level diff (interned), `char` for the inner
/// character-level diff inside `_fancy_replace`.
pub struct SequenceMatcher<T: Eq + Hash + Clone> {
    a: Vec<T>,
    b: Vec<T>,
    autojunk: bool,

    /// `b[j] → all j` (with popular elements stripped if `autojunk` and
    /// `len(b) >= 200`). Lazily computed; invalidated by `set_seq2`.
    b2j: Option<HashMap<T, Vec<usize>>>,

    /// Cached matching-block list (Python's `self.matching_blocks`).
    /// Invalidated by both `set_seq1` and `set_seq2`.
    matching_blocks: Option<Vec<Match>>,

    /// Cached opcode list (Python's `self.opcodes`). Invalidated by
    /// `set_seq1` and `set_seq2`.
    opcodes: Option<Vec<OpCode>>,

    /// `b` viewed as a multiset (count per distinct element). Used by
    /// `quick_ratio`; invalidated by `set_seq2`.
    fullbcount: Option<HashMap<T, usize>>,
}

impl<T: Eq + Hash + Clone> Default for SequenceMatcher<T> {
    fn default() -> Self {
        Self::new(true)
    }
}

impl<T: Eq + Hash + Clone> SequenceMatcher<T> {
    /// Construct an empty matcher with the given autojunk setting.
    /// Matches Python's `SequenceMatcher(isjunk=None, autojunk=True)`
    /// when called as `new(true)` (we don't expose isjunk because the
    /// cascade never uses it).
    pub fn new(autojunk: bool) -> Self {
        Self {
            a: Vec::new(),
            b: Vec::new(),
            autojunk,
            b2j: None,
            matching_blocks: None,
            opcodes: None,
            fullbcount: None,
        }
    }

    /// Set both sequences in one call.
    pub fn set_seqs(&mut self, a: Vec<T>, b: Vec<T>) {
        self.set_seq2(b);
        self.set_seq1(a);
    }

    /// Replace `a`. Invalidates matching_blocks/opcodes but NOT `b2j`
    /// or `fullbcount` (which are functions of `b` only). Mirrors
    /// `SequenceMatcher.set_seq1`.
    pub fn set_seq1(&mut self, a: Vec<T>) {
        if a == self.a {
            return;
        }
        self.a = a;
        self.matching_blocks = None;
        self.opcodes = None;
    }

    /// Replace `b`. Invalidates everything that depends on it.
    pub fn set_seq2(&mut self, b: Vec<T>) {
        if b == self.b {
            return;
        }
        self.b = b;
        self.matching_blocks = None;
        self.opcodes = None;
        self.b2j = None;
        self.fullbcount = None;
    }

    /// Lazily build `b2j` (with autojunk filtering). Mirrors
    /// `__chain_b`.
    fn chain_b(&mut self) {
        if self.b2j.is_some() {
            return;
        }
        let mut b2j: HashMap<T, Vec<usize>> = HashMap::new();
        for (i, elt) in self.b.iter().enumerate() {
            b2j.entry(elt.clone()).or_default().push(i);
        }

        let n = self.b.len();
        if self.autojunk && n >= 200 {
            let ntest = n / 100 + 1;
            let popular: Vec<T> = b2j
                .iter()
                .filter(|(_, idxs)| idxs.len() > ntest)
                .map(|(elt, _)| elt.clone())
                .collect();
            for elt in popular {
                b2j.remove(&elt);
            }
        }

        self.b2j = Some(b2j);
    }

    /// `find_longest_match` from `difflib.py:305`. Returns the longest
    /// contiguous matching block in `a[alo..ahi]` × `b[blo..bhi]`,
    /// with ties broken to favour the earliest start in `a` (then in
    /// `b`). With no `isjunk`, the extension-and-suck-up phases at the
    /// end of the Python implementation simplify to the single
    /// non-junk extension; we keep both phases written out for fidelity
    /// in case `isjunk` is ever wired in.
    pub fn find_longest_match(
        &mut self,
        alo: usize,
        ahi: usize,
        blo: usize,
        bhi: usize,
    ) -> Match {
        self.chain_b();
        let b2j = self.b2j.as_ref().expect("chain_b set b2j");

        let mut besti = alo;
        let mut bestj = blo;
        let mut bestsize: usize = 0;

        let nothing: Vec<usize> = Vec::new();
        let mut j2len: HashMap<usize, usize> = HashMap::new();

        for i in alo..ahi {
            let positions = b2j.get(&self.a[i]).unwrap_or(&nothing);
            let mut newj2len: HashMap<usize, usize> = HashMap::new();
            for &j in positions {
                if j < blo {
                    continue;
                }
                if j >= bhi {
                    // b2j positions are stored in insertion order,
                    // i.e. ascending — once we exceed bhi we're done.
                    break;
                }
                let prev_len = if j == 0 {
                    0
                } else {
                    j2len.get(&(j - 1)).copied().unwrap_or(0)
                };
                let k = prev_len + 1;
                newj2len.insert(j, k);
                if k > bestsize {
                    besti = i + 1 - k;
                    bestj = j + 1 - k;
                    bestsize = k;
                }
            }
            j2len = newj2len;
        }

        // Extend the match by non-junk elements on each end. With no
        // isjunk, "junk" is empty, so this phase covers all matching
        // adjacency.
        while besti > alo
            && bestj > blo
            && self.a[besti - 1] == self.b[bestj - 1]
        {
            besti -= 1;
            bestj -= 1;
            bestsize += 1;
        }
        while besti + bestsize < ahi
            && bestj + bestsize < bhi
            && self.a[besti + bestsize] == self.b[bestj + bestsize]
        {
            bestsize += 1;
        }

        // Suck-up-junk phase would go here; with no isjunk it's a
        // no-op so we elide it.

        Match {
            a_start: besti,
            b_start: bestj,
            size: bestsize,
        }
    }

    /// `get_matching_blocks` from `difflib.py:421`. Recursive R/O
    /// decomposition implemented with an explicit queue (per Python's
    /// comment about extreme cases blowing the recursion limit).
    pub fn get_matching_blocks(&mut self) -> Vec<Match> {
        if let Some(blocks) = &self.matching_blocks {
            return blocks.clone();
        }

        let la = self.a.len();
        let lb = self.b.len();
        let mut queue: Vec<(usize, usize, usize, usize)> = vec![(0, la, 0, lb)];
        let mut matching_blocks: Vec<Match> = Vec::new();

        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let m = self.find_longest_match(alo, ahi, blo, bhi);
            if m.size > 0 {
                matching_blocks.push(m);
                if alo < m.a_start && blo < m.b_start {
                    queue.push((alo, m.a_start, blo, m.b_start));
                }
                if m.a_start + m.size < ahi && m.b_start + m.size < bhi {
                    queue.push((m.a_start + m.size, ahi, m.b_start + m.size, bhi));
                }
            }
        }

        // Sort by (a_start, b_start, size).
        matching_blocks.sort_by_key(|m| (m.a_start, m.b_start, m.size));

        // Collapse adjacent equal blocks (added in CPython 2.5).
        let mut i1: usize = 0;
        let mut j1: usize = 0;
        let mut k1: usize = 0;
        let mut non_adjacent: Vec<Match> = Vec::new();
        for m in &matching_blocks {
            let (i2, j2, k2) = (m.a_start, m.b_start, m.size);
            if i1 + k1 == i2 && j1 + k1 == j2 {
                k1 += k2;
            } else {
                if k1 > 0 {
                    non_adjacent.push(Match {
                        a_start: i1,
                        b_start: j1,
                        size: k1,
                    });
                }
                i1 = i2;
                j1 = j2;
                k1 = k2;
            }
        }
        if k1 > 0 {
            non_adjacent.push(Match {
                a_start: i1,
                b_start: j1,
                size: k1,
            });
        }

        // Sentinel block at the end (size 0, positions = la, lb). The
        // opcode walker reads it as an upper bound.
        non_adjacent.push(Match {
            a_start: la,
            b_start: lb,
            size: 0,
        });

        self.matching_blocks = Some(non_adjacent.clone());
        non_adjacent
    }

    /// `get_opcodes` from `difflib.py:492`. Walks the matching blocks
    /// and fills the gaps with `replace` / `delete` / `insert` tags.
    pub fn get_opcodes(&mut self) -> Vec<OpCode> {
        if let Some(codes) = &self.opcodes {
            return codes.clone();
        }
        let blocks = self.get_matching_blocks();
        let mut answer: Vec<OpCode> = Vec::new();
        let mut i: usize = 0;
        let mut j: usize = 0;
        for block in blocks {
            let (ai, bj, size) = (block.a_start, block.b_start, block.size);
            let tag = if i < ai && j < bj {
                Some(OpTag::Replace)
            } else if i < ai {
                Some(OpTag::Delete)
            } else if j < bj {
                Some(OpTag::Insert)
            } else {
                None
            };
            if let Some(tag) = tag {
                answer.push(OpCode {
                    tag,
                    i1: i,
                    i2: ai,
                    j1: j,
                    j2: bj,
                });
            }
            i = ai + size;
            j = bj + size;
            if size > 0 {
                answer.push(OpCode {
                    tag: OpTag::Equal,
                    i1: ai,
                    i2: i,
                    j1: bj,
                    j2: j,
                });
            }
        }
        self.opcodes = Some(answer.clone());
        answer
    }

    /// `ratio` from `difflib.py:597`. `2 * matches / (la + lb)`.
    pub fn ratio(&mut self) -> f64 {
        let matches: usize = self.get_matching_blocks().iter().map(|m| m.size).sum();
        calculate_ratio(matches, self.a.len() + self.b.len())
    }

    /// `quick_ratio` from `difflib.py:622`. Upper bound on `ratio`
    /// computed as the multiset intersection size between `a` and `b`.
    pub fn quick_ratio(&mut self) -> f64 {
        if self.fullbcount.is_none() {
            let mut counts: HashMap<T, usize> = HashMap::new();
            for elt in &self.b {
                *counts.entry(elt.clone()).or_insert(0) += 1;
            }
            self.fullbcount = Some(counts);
        }
        let fullbcount = self.fullbcount.as_ref().expect("just populated");

        let mut avail: HashMap<&T, isize> = HashMap::new();
        let mut matches: usize = 0;
        for elt in &self.a {
            let numb = match avail.get(elt) {
                Some(&v) => v,
                None => *fullbcount.get(elt).unwrap_or(&0) as isize,
            };
            avail.insert(elt, numb - 1);
            if numb > 0 {
                matches += 1;
            }
        }
        calculate_ratio(matches, self.a.len() + self.b.len())
    }

    /// `real_quick_ratio` from `difflib.py:651`. Very cheap upper
    /// bound: `2 * min(la, lb) / (la + lb)`.
    pub fn real_quick_ratio(&self) -> f64 {
        let la = self.a.len();
        let lb = self.b.len();
        calculate_ratio(la.min(lb), la + lb)
    }
}

/// `_calculate_ratio` from `difflib.py`. Defined as `2*matches / total`
/// with the `total == 0 → 1.0` edge case.
fn calculate_ratio(matches: usize, total: usize) -> f64 {
    if total == 0 {
        1.0
    } else {
        2.0 * matches as f64 / total as f64
    }
}

// =====================================================================
// Differ — the line-level formatter that drives the cascade.
// =====================================================================

/// `Differ` from `difflib.py:724`. Walks the outer opcodes and dispatches
/// each 'replace' block through `_fancy_replace`. Only the operations
/// the cascade consumes (Keep/Delete/Insert) are produced; the `'? '`
/// hint lines Python emits for human readers are dropped.
struct Differ {
    /// Reusable inner matcher for `_fancy_replace`'s character-level
    /// similarity probe. Constructed once per top-level diff and reset
    /// per pair via `set_seq1`/`set_seq2`.
    char_cruncher: SequenceMatcher<char>,
}

impl Differ {
    fn new() -> Self {
        Self {
            // `Differ.__init__` passes `charjunk=None` by default and
            // the inner matcher inherits autojunk=True. Token strings
            // are usually well under 200 chars, so autojunk rarely
            // fires; we keep it on for parity with Python.
            char_cruncher: SequenceMatcher::new(true),
        }
    }

    // Signature mirrors Python's `_fancy_replace(a, alo, ahi, b, blo,
    // bhi)` — same arg order, plus the `out` sink we append into. The
    // arg count is intrinsic to the source we're porting, not a
    // design choice we can shrink.
    #[allow(clippy::too_many_arguments)]
    fn fancy_replace(
        &mut self,
        a: &[String],
        alo: usize,
        ahi: usize,
        b: &[String],
        blo: usize,
        bhi: usize,
        out: &mut Vec<DiffOp>,
    ) {
        const CUTOFF: f64 = 0.75;

        let mut best_ratio: f64 = 0.74;
        let mut best_i: usize = 0;
        let mut best_j: usize = 0;
        let mut eqi: Option<usize> = None;
        let mut eqj: Option<usize> = None;

        // Outer loop on j (Python's `for j in range(blo, bhi)`),
        // inner on i. The tie-breaker that falls out of this order:
        // among (i, j) pairs with the same ratio, the smallest j wins,
        // then the smallest i. Differ.compare matches this.
        for (j_offset, bj) in b[blo..bhi].iter().enumerate() {
            let j = blo + j_offset;
            let bj_chars: Vec<char> = bj.chars().collect();
            self.char_cruncher.set_seq2(bj_chars);
            for (i_offset, ai) in a[alo..ahi].iter().enumerate() {
                let i = alo + i_offset;
                if ai == bj {
                    if eqi.is_none() {
                        eqi = Some(i);
                        eqj = Some(j);
                    }
                    continue;
                }
                let ai_chars: Vec<char> = ai.chars().collect();
                self.char_cruncher.set_seq1(ai_chars);
                // Mirror Python's short-circuit cascade exactly:
                // real_quick_ratio → quick_ratio → ratio. The strict-
                // `>` (not `>=`) preserves the "first one wins" tie
                // semantics.
                if self.char_cruncher.real_quick_ratio() > best_ratio
                    && self.char_cruncher.quick_ratio() > best_ratio
                    && self.char_cruncher.ratio() > best_ratio
                {
                    best_ratio = self.char_cruncher.ratio();
                    best_i = i;
                    best_j = j;
                }
            }
        }

        let mut synch_is_identical: bool;
        let synch_i;
        let synch_j;

        if best_ratio < CUTOFF {
            // No "pretty close" pair found.
            if let (Some(ei), Some(ej)) = (eqi, eqj) {
                // Fall back to the first identical pair (Python:
                // "synch up on that").
                synch_i = ei;
                synch_j = ej;
                synch_is_identical = true;
            } else {
                // No close OR identical pair — plain replace.
                self.plain_replace(a, alo, ahi, b, blo, bhi, out);
                return;
            }
        } else {
            // Close pair wins, identical pair (if any) is discarded
            // (Python: `eqi = None`).
            synch_i = best_i;
            synch_j = best_j;
            synch_is_identical = false;
        }

        // Before the synch.
        self.fancy_helper(a, alo, synch_i, b, blo, synch_j, out);

        // The synch line itself.
        if synch_is_identical {
            out.push(DiffOp::Keep(a[synch_i].clone()));
        } else {
            // `_qformat` would emit '- aelt', '? hint', '+ belt',
            // '? hint'. The cascade strips '?' lines; we only emit
            // the '- ' and '+ ' for parity.
            out.push(DiffOp::Delete(a[synch_i].clone()));
            out.push(DiffOp::Insert(b[synch_j].clone()));
            // Silence unused-mut warning when only the close-pair branch
            // assigns to synch_is_identical above.
            let _ = &mut synch_is_identical;
        }

        // After the synch.
        self.fancy_helper(a, synch_i + 1, ahi, b, synch_j + 1, bhi, out);
    }

    #[allow(clippy::too_many_arguments)]
    fn fancy_helper(
        &mut self,
        a: &[String],
        alo: usize,
        ahi: usize,
        b: &[String],
        blo: usize,
        bhi: usize,
        out: &mut Vec<DiffOp>,
    ) {
        if alo < ahi {
            if blo < bhi {
                self.fancy_replace(a, alo, ahi, b, blo, bhi, out);
            } else {
                for ai in &a[alo..ahi] {
                    out.push(DiffOp::Delete(ai.clone()));
                }
            }
        } else if blo < bhi {
            for bj in &b[blo..bhi] {
                out.push(DiffOp::Insert(bj.clone()));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn plain_replace(
        &self,
        a: &[String],
        alo: usize,
        ahi: usize,
        b: &[String],
        blo: usize,
        bhi: usize,
        out: &mut Vec<DiffOp>,
    ) {
        debug_assert!(alo < ahi && blo < bhi);
        // Python: "dump the shorter block first -- reduces the burden
        // on short-term memory if the blocks are of very different
        // sizes". The condition is strict: equal-length replaces emit
        // deletes first.
        if (bhi - blo) < (ahi - alo) {
            for bj in &b[blo..bhi] {
                out.push(DiffOp::Insert(bj.clone()));
            }
            for ai in &a[alo..ahi] {
                out.push(DiffOp::Delete(ai.clone()));
            }
        } else {
            for ai in &a[alo..ahi] {
                out.push(DiffOp::Delete(ai.clone()));
            }
            for bj in &b[blo..bhi] {
                out.push(DiffOp::Insert(bj.clone()));
            }
        }
    }
}

/// Drop-in replacement for `Differ().compare(text_prev, text_curr)`.
///
/// The token sequences are interned into `u32` IDs for fast outer
/// matching (matching the Myers code path's approach in `diff.rs`).
/// The inner `_fancy_replace` uses the original strings for
/// character-level similarity probing.
pub fn differ_compare(text_prev: &[String], text_curr: &[String]) -> Vec<DiffOp> {
    // Intern token strings to u32 IDs. Same shape as
    // `diff::intern_sequences` but inlined here so we own the IDs and
    // can drop the lookup table after `SequenceMatcher` is done.
    let mut id_table: HashMap<String, u32> = HashMap::new();
    let mut intern = |s: &str| -> u32 {
        if let Some(&id) = id_table.get(s) {
            return id;
        }
        let id = id_table.len() as u32;
        id_table.insert(s.to_string(), id);
        id
    };
    let a: Vec<u32> = text_prev.iter().map(|s| intern(s.as_str())).collect();
    let b: Vec<u32> = text_curr.iter().map(|s| intern(s.as_str())).collect();

    let mut sm = SequenceMatcher::<u32>::new(true);
    sm.set_seqs(a, b);
    let opcodes = sm.get_opcodes();

    let mut differ = Differ::new();
    let mut out: Vec<DiffOp> = Vec::new();
    for op in opcodes {
        match op.tag {
            OpTag::Equal => {
                for value in &text_prev[op.i1..op.i2] {
                    out.push(DiffOp::Keep(value.clone()));
                }
            }
            OpTag::Delete => {
                for value in &text_prev[op.i1..op.i2] {
                    out.push(DiffOp::Delete(value.clone()));
                }
            }
            OpTag::Insert => {
                for value in &text_curr[op.j1..op.j2] {
                    out.push(DiffOp::Insert(value.clone()));
                }
            }
            OpTag::Replace => {
                differ.fancy_replace(
                    text_prev, op.i1, op.i2, text_curr, op.j1, op.j2, &mut out,
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> String {
        x.to_string()
    }

    fn keeps(values: &[&str]) -> Vec<DiffOp> {
        values.iter().map(|v| DiffOp::Keep(s(v))).collect()
    }

    #[test]
    fn empty_inputs_yield_empty_diff() {
        assert_eq!(differ_compare(&[], &[]), Vec::<DiffOp>::new());
    }

    #[test]
    fn empty_prev_yields_all_inserts() {
        let prev: Vec<String> = vec![];
        let curr = vec![s("a"), s("b"), s("c")];
        assert_eq!(
            differ_compare(&prev, &curr),
            vec![
                DiffOp::Insert(s("a")),
                DiffOp::Insert(s("b")),
                DiffOp::Insert(s("c")),
            ]
        );
    }

    #[test]
    fn empty_curr_yields_all_deletes() {
        let prev = vec![s("a"), s("b"), s("c")];
        let curr: Vec<String> = vec![];
        assert_eq!(
            differ_compare(&prev, &curr),
            vec![
                DiffOp::Delete(s("a")),
                DiffOp::Delete(s("b")),
                DiffOp::Delete(s("c")),
            ]
        );
    }

    #[test]
    fn identical_sequences_are_all_keeps() {
        let v = vec![s("foo"), s("bar"), s("baz")];
        assert_eq!(differ_compare(&v, &v), keeps(&["foo", "bar", "baz"]));
    }

    #[test]
    fn single_token_substitution_in_middle() {
        // a = [foo, bar, baz], b = [foo, qux, baz]. 'bar' and 'qux'
        // share no characters → plain replace, deletes first.
        let prev = vec![s("foo"), s("bar"), s("baz")];
        let curr = vec![s("foo"), s("qux"), s("baz")];
        assert_eq!(
            differ_compare(&prev, &curr),
            vec![
                DiffOp::Keep(s("foo")),
                DiffOp::Delete(s("bar")),
                DiffOp::Insert(s("qux")),
                DiffOp::Keep(s("baz")),
            ]
        );
    }

    #[test]
    fn pure_insertion_in_middle() {
        let prev = vec![s("a"), s("c")];
        let curr = vec![s("a"), s("b"), s("c")];
        assert_eq!(
            differ_compare(&prev, &curr),
            vec![
                DiffOp::Keep(s("a")),
                DiffOp::Insert(s("b")),
                DiffOp::Keep(s("c")),
            ]
        );
    }

    #[test]
    fn pure_deletion_in_middle() {
        let prev = vec![s("a"), s("b"), s("c")];
        let curr = vec![s("a"), s("c")];
        assert_eq!(
            differ_compare(&prev, &curr),
            vec![
                DiffOp::Keep(s("a")),
                DiffOp::Delete(s("b")),
                DiffOp::Keep(s("c")),
            ]
        );
    }

    #[test]
    fn close_pair_replace_orders_dash_then_plus() {
        // 'world' and 'worle' differ by one char → fancy_replace finds
        // ratio > 0.75 → emits the '- world' / '+ worle' pair (no
        // intervening reordering since the replace block has only one
        // element on each side).
        let prev = vec![s("hello"), s("world")];
        let curr = vec![s("hello"), s("worle")];
        let ops = differ_compare(&prev, &curr);
        assert_eq!(
            ops,
            vec![
                DiffOp::Keep(s("hello")),
                DiffOp::Delete(s("world")),
                DiffOp::Insert(s("worle")),
            ]
        );
    }

    #[test]
    fn fancy_replace_can_interleave_dash_plus() {
        // Replace block where the close pair is at the END of one side
        // and the beginning of the other. Python's _fancy_replace recurses
        // around the synch point, which produces an INTERLEAVED dash/plus
        // ordering (vs plain replace which would emit all dashes then all
        // pluses). a=[hello] vs b=[world, hella]: the close pair is
        // (hello, hella) at positions (0, 1). Python emits + world, then
        // the synch (- hello, + hella).
        let prev = vec![s("hello")];
        let curr = vec![s("world"), s("hella")];
        let ops = differ_compare(&prev, &curr);
        assert_eq!(
            ops,
            vec![
                DiffOp::Insert(s("world")),
                DiffOp::Delete(s("hello")),
                DiffOp::Insert(s("hella")),
            ]
        );
    }

    #[test]
    fn duplicate_tokens_are_matched_left_to_right() {
        // R/O's tie-breaker (earliest-start-in-a, then earliest-start-in-b)
        // means a = [the, cat, the, rat], b = [the, dog, the, rat] matches
        // the LEFT 'the' in a to the LEFT 'the' in b. The right 'the rat'
        // contiguous block in both sequences is the longest match.
        let prev = vec![s("the"), s("cat"), s("the"), s("rat")];
        let curr = vec![s("the"), s("dog"), s("the"), s("rat")];
        let ops = differ_compare(&prev, &curr);
        // 'the rat' (positions 2..4 in both) is the unique longest
        // contiguous block (length 2). Then recursion on the left half
        // finds 'the' as the next longest. Result:
        //   keep the, delete cat, insert dog, keep the, keep rat.
        assert_eq!(
            ops,
            vec![
                DiffOp::Keep(s("the")),
                DiffOp::Delete(s("cat")),
                DiffOp::Insert(s("dog")),
                DiffOp::Keep(s("the")),
                DiffOp::Keep(s("rat")),
            ]
        );
    }

    #[test]
    fn dissimilar_replace_with_unequal_lengths_inserts_first() {
        // Plain replace, b longer than a → Python emits the SHORTER
        // (a side, deletes) first per "dump the shorter block first".
        let prev = vec![s("a"), s("b")];
        let curr = vec![s("x"), s("y"), s("z")];
        let ops = differ_compare(&prev, &curr);
        // Note no close-match (single-char strings, real_quick_ratio
        // would be 1.0 but ratio is 0.0 — no shared characters).
        assert_eq!(
            ops,
            vec![
                DiffOp::Delete(s("a")),
                DiffOp::Delete(s("b")),
                DiffOp::Insert(s("x")),
                DiffOp::Insert(s("y")),
                DiffOp::Insert(s("z")),
            ]
        );
    }

    #[test]
    fn dissimilar_replace_with_longer_a_inserts_first() {
        // Plain replace, a longer than b → emit b (shorter, '+' side)
        // first.
        let prev = vec![s("a"), s("b"), s("c")];
        let curr = vec![s("x"), s("y")];
        let ops = differ_compare(&prev, &curr);
        assert_eq!(
            ops,
            vec![
                DiffOp::Insert(s("x")),
                DiffOp::Insert(s("y")),
                DiffOp::Delete(s("a")),
                DiffOp::Delete(s("b")),
                DiffOp::Delete(s("c")),
            ]
        );
    }

    #[test]
    fn transcript_reconstructs_both_sequences() {
        let cases: &[(&[&str], &[&str])] = &[
            (&[], &["x"]),
            (&["x"], &[]),
            (&["a", "b", "c"], &["a", "b", "c"]),
            (&["the", "cat"], &["the", "dog"]),
            (&["foo", "bar"], &["bar", "foo"]),
            (&["a", "b", "c", "d"], &["e", "f", "g", "h"]),
            (&["1", "2", "3"], &["3", "2", "1"]),
        ];
        for (a_slice, b_slice) in cases {
            let a: Vec<String> = a_slice.iter().map(|s| (*s).to_string()).collect();
            let b: Vec<String> = b_slice.iter().map(|s| (*s).to_string()).collect();
            let ops = differ_compare(&a, &b);
            let reconstructed_b: Vec<String> = ops
                .iter()
                .filter_map(|op| match op {
                    DiffOp::Keep(v) | DiffOp::Insert(v) => Some(v.clone()),
                    DiffOp::Delete(_) => None,
                })
                .collect();
            let reconstructed_a: Vec<String> = ops
                .iter()
                .filter_map(|op| match op {
                    DiffOp::Keep(v) | DiffOp::Delete(v) => Some(v.clone()),
                    DiffOp::Insert(_) => None,
                })
                .collect();
            assert_eq!(
                reconstructed_a, a,
                "transcript should reconstruct a: {a_slice:?} -> {b_slice:?}"
            );
            assert_eq!(
                reconstructed_b, b,
                "transcript should reconstruct b: {a_slice:?} -> {b_slice:?}"
            );
        }
    }

    #[test]
    fn sequence_matcher_simple_lcs_example() {
        // From difflib doctest: a = "qabxcd", b = "abycdf".
        // get_opcodes: delete, equal, replace, equal, insert.
        let a: Vec<char> = "qabxcd".chars().collect();
        let b: Vec<char> = "abycdf".chars().collect();
        let mut sm = SequenceMatcher::<char>::new(true);
        sm.set_seqs(a, b);
        let opcodes = sm.get_opcodes();
        let tags: Vec<OpTag> = opcodes.iter().map(|o| o.tag).collect();
        assert_eq!(
            tags,
            vec![
                OpTag::Delete,
                OpTag::Equal,
                OpTag::Replace,
                OpTag::Equal,
                OpTag::Insert,
            ]
        );
    }

    #[test]
    fn ratio_doctest_matches_python() {
        // From difflib doctest: SequenceMatcher(None, "abcd", "bcde").ratio() == 0.75.
        let a: Vec<char> = "abcd".chars().collect();
        let b: Vec<char> = "bcde".chars().collect();
        let mut sm = SequenceMatcher::<char>::new(true);
        sm.set_seqs(a, b);
        let r = sm.ratio();
        assert!((r - 0.75).abs() < 1e-9, "ratio = {r}");
    }

    #[test]
    fn find_longest_match_doctest_matches_python() {
        // From difflib doctest: " abcd" vs "abcd abcd" → Match(0, 4, 5).
        let a: Vec<char> = " abcd".chars().collect();
        let b: Vec<char> = "abcd abcd".chars().collect();
        let mut sm = SequenceMatcher::<char>::new(true);
        sm.set_seqs(a, b);
        let m = sm.find_longest_match(0, 5, 0, 9);
        assert_eq!(
            m,
            Match {
                a_start: 0,
                b_start: 4,
                size: 5,
            }
        );
    }

    #[test]
    fn get_matching_blocks_doctest_matches_python() {
        // From difflib doctest: "abxcd" vs "abcd" →
        // [Match(0,0,2), Match(3,2,2), Match(5,4,0)].
        let a: Vec<char> = "abxcd".chars().collect();
        let b: Vec<char> = "abcd".chars().collect();
        let mut sm = SequenceMatcher::<char>::new(true);
        sm.set_seqs(a, b);
        let blocks = sm.get_matching_blocks();
        assert_eq!(
            blocks,
            vec![
                Match { a_start: 0, b_start: 0, size: 2 },
                Match { a_start: 3, b_start: 2, size: 2 },
                Match { a_start: 5, b_start: 4, size: 0 },
            ]
        );
    }

    #[test]
    fn autojunk_does_not_break_correctness_on_long_sequences() {
        // Autojunk is primarily a performance optimization: it strips
        // popular elements from `b2j` so the inner scan in
        // `find_longest_match` doesn't iterate over them. The extension
        // phase can still pick them up by adjacency (since `bjunk` is
        // empty when no `isjunk` is set, the `not isbjunk(...)` check
        // is always true). So the *result* of a match is usually the
        // same with or without autojunk; what changes is the search
        // path.
        //
        // What we DO want to verify is that autojunk doesn't break
        // correctness on a >200 sequence where a long contiguous block
        // is present. Cross-checked with Python:
        //   >>> SequenceMatcher(None, [999]*100 + [42], [999]*200 + [42])
        //   ...   .find_longest_match(0, 101, 0, 201)
        //   Match(a=0, b=100, size=101)
        let mut b: Vec<u32> = vec![999; 200];
        b.push(42);
        let a: Vec<u32> = {
            let mut v = vec![999; 100];
            v.push(42);
            v
        };
        let mut sm = SequenceMatcher::<u32>::new(true);
        sm.set_seqs(a.clone(), b.clone());
        let m = sm.find_longest_match(0, a.len(), 0, b.len());
        assert_eq!(
            m,
            Match {
                a_start: 0,
                b_start: 100,
                size: 101
            }
        );

        // Same answer with autojunk OFF.
        let mut sm2 = SequenceMatcher::<u32>::new(false);
        sm2.set_seqs(a, b);
        let m2 = sm2.find_longest_match(0, sm2.a.len(), 0, sm2.b.len());
        assert_eq!(m, m2);
    }

    #[test]
    fn quick_ratio_is_upper_bound_on_ratio() {
        let prev = [s("a"), s("b"), s("c"), s("a")];
        let curr = [s("c"), s("b"), s("a"), s("d")];
        let a: Vec<char> = prev.join("").chars().collect();
        let b: Vec<char> = curr.join("").chars().collect();
        let mut sm = SequenceMatcher::<char>::new(true);
        sm.set_seqs(a, b);
        let r = sm.ratio();
        let qr = sm.quick_ratio();
        let rqr = sm.real_quick_ratio();
        assert!(qr >= r, "quick_ratio={qr} < ratio={r}");
        assert!(rqr >= qr, "real_quick_ratio={rqr} < quick_ratio={qr}");
    }
}
