//! Myers diff (1986) over `&[u32]` token id sequences.
//!
//! Replaces Python `difflib.Differ().compare(text_prev, text_curr)`
//! used inside `analyse_words_in_sentences` (`wikiwho.py:631`). See
//! `ALGORITHM.md §6` for the parity caveat: `Differ` uses LCS with an
//! anchor-heuristic, Myers uses the shortest-edit-script. On duplicate
//! tokens the two can pick different positions to match — observable
//! as different `o_rev_id` on otherwise-identical-looking tokens.
//! Acceptance threshold: <0.1% of tokens diverge across the parity
//! corpus.
//!
//! Complexity: O((N+M)·D) time, O(D·(N+M)) space, where D is the edit
//! distance. The diff is called per (concatenated) batch of unmatched
//! curr+prev sentences, so for typical edits both N and D are small.
//! Linear-space refinements (Hirschberg) are a follow-up if Obama-class
//! processing shows the trace memory in profiles.

/// One token-level edit. The contained `u32` is the interned token id
/// (see [`intern_sequences`]). For `Keep` and `Delete` it is the prev
/// token's id; for `Insert` it is the curr token's id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffOp {
    Keep(u32),
    Delete(u32),
    Insert(u32),
}

/// Compute the Myers shortest-edit-script transcript over two sequences
/// of interned token ids.
///
/// Returns the edit ops in input order (left-to-right along the merged
/// timeline): a `Keep`/`Delete` consumes one element of `a`, a
/// `Keep`/`Insert` consumes one element of `b`. The transcript length
/// is exactly `n_keeps + n_deletes + n_inserts`, mirroring
/// `difflib.Differ().compare(a, b)` minus the `'?'` hint lines.
pub fn myers_diff(a: &[u32], b: &[u32]) -> Vec<DiffOp> {
    let n = a.len();
    let m = b.len();

    if n == 0 && m == 0 {
        return Vec::new();
    }
    if n == 0 {
        return b.iter().map(|&v| DiffOp::Insert(v)).collect();
    }
    if m == 0 {
        return a.iter().map(|&v| DiffOp::Delete(v)).collect();
    }

    // The V array tracks the furthest-reaching x-coordinate per
    // diagonal k = x - y. k ranges over [-(n+m), +(n+m)]; we shift by
    // `offset` for vector indexing.
    let max = n + m;
    let offset = max as isize;
    let v_len = 2 * max + 1;
    let mut v: Vec<isize> = vec![0; v_len];
    // One snapshot of V per edit distance reached, used by `backtrack`.
    let mut trace: Vec<Vec<isize>> = Vec::with_capacity(max + 1);

    for d in 0..=(max as isize) {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let idx = (k + offset) as usize;
            // Choose the predecessor that extends furthest along the
            // diagonal: "down" (insert from b, x unchanged) vs "right"
            // (delete from a, x + 1). At the edges (k == -d or k == d)
            // only one direction is reachable.
            let mut x: isize = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) {
                v[idx + 1]
            } else {
                v[idx - 1] + 1
            };
            let mut y: isize = x - k;

            // Follow the snake: consume matching pairs.
            while (x as usize) < n
                && (y as usize) < m
                && a[x as usize] == b[y as usize]
            {
                x += 1;
                y += 1;
            }

            v[idx] = x;

            if x as usize >= n && y as usize >= m {
                return backtrack(a, b, &trace, offset, d);
            }

            k += 2;
        }
    }

    // The loop is guaranteed to terminate within d ≤ n + m. Reaching
    // here means a contract violation in the algorithm itself.
    unreachable!("Myers diff did not converge within max edit distance")
}

fn backtrack(
    a: &[u32],
    b: &[u32],
    trace: &[Vec<isize>],
    offset: isize,
    end_d: isize,
) -> Vec<DiffOp> {
    let mut ops: Vec<DiffOp> = Vec::new();
    let mut x = a.len() as isize;
    let mut y = b.len() as isize;

    for d in (0..=end_d).rev() {
        let v = &trace[d as usize];
        let k = x - y;
        let idx = (k + offset) as usize;

        // Which neighbour did we come from? Same rule as the forward
        // search: from above (k+1) if at the lower edge or the upper
        // diagonal has a shorter furthest-x; otherwise from the left
        // (k-1).
        let prev_k = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) {
            k + 1
        } else {
            k - 1
        };
        let prev_idx = (prev_k + offset) as usize;
        let prev_x = v[prev_idx];
        let prev_y = prev_x - prev_k;

        // Walk the diagonal snake we found at this d level.
        while x > prev_x && y > prev_y {
            ops.push(DiffOp::Keep(a[(x - 1) as usize]));
            x -= 1;
            y -= 1;
        }

        if d > 0 {
            if x == prev_x {
                ops.push(DiffOp::Insert(b[(y - 1) as usize]));
                y -= 1;
            } else {
                ops.push(DiffOp::Delete(a[(x - 1) as usize]));
                x -= 1;
            }
        }
    }

    ops.reverse();
    ops
}

/// Intern two slices of string tokens into a shared id table.
///
/// `text_prev` and `text_curr` are returned as `Vec<u32>` aligned with
/// their inputs. Equal strings hash to the same id across both slices,
/// which is exactly what Myers wants. The returned `Vec<String>` is the
/// id→value table — handy for debug formatting but otherwise unused.
pub fn intern_sequences(text_prev: &[String], text_curr: &[String]) -> (Vec<u32>, Vec<u32>, Vec<String>) {
    use std::collections::HashMap;
    let mut table: HashMap<String, u32> = HashMap::new();
    let mut values: Vec<String> = Vec::new();
    let mut intern = |s: &str| -> u32 {
        if let Some(&id) = table.get(s) {
            return id;
        }
        let id = values.len() as u32;
        values.push(s.to_string());
        table.insert(s.to_string(), id);
        id
    };
    let a: Vec<u32> = text_prev.iter().map(|s| intern(s.as_str())).collect();
    let b: Vec<u32> = text_curr.iter().map(|s| intern(s.as_str())).collect();
    (a, b, values)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keeps(values: &[u32]) -> Vec<DiffOp> {
        values.iter().map(|&v| DiffOp::Keep(v)).collect()
    }

    #[test]
    fn empty_inputs_yield_empty_transcript() {
        assert_eq!(myers_diff(&[], &[]), Vec::<DiffOp>::new());
    }

    #[test]
    fn empty_prev_yields_all_inserts() {
        assert_eq!(
            myers_diff(&[], &[1, 2, 3]),
            vec![DiffOp::Insert(1), DiffOp::Insert(2), DiffOp::Insert(3)]
        );
    }

    #[test]
    fn empty_curr_yields_all_deletes() {
        assert_eq!(
            myers_diff(&[1, 2, 3], &[]),
            vec![DiffOp::Delete(1), DiffOp::Delete(2), DiffOp::Delete(3)]
        );
    }

    #[test]
    fn identical_sequences_yield_all_keeps() {
        assert_eq!(myers_diff(&[1, 2, 3], &[1, 2, 3]), keeps(&[1, 2, 3]));
    }

    #[test]
    fn single_substitution_in_middle() {
        // a = [1, 2, 3], b = [1, 4, 3]: keep 1, delete 2, insert 4, keep 3.
        let ops = myers_diff(&[1, 2, 3], &[1, 4, 3]);
        assert_eq!(
            ops,
            vec![
                DiffOp::Keep(1),
                DiffOp::Delete(2),
                DiffOp::Insert(4),
                DiffOp::Keep(3),
            ]
        );
    }

    #[test]
    fn pure_insertion_in_middle() {
        // a = [1, 3], b = [1, 2, 3]: keep 1, insert 2, keep 3.
        let ops = myers_diff(&[1, 3], &[1, 2, 3]);
        assert_eq!(
            ops,
            vec![DiffOp::Keep(1), DiffOp::Insert(2), DiffOp::Keep(3)]
        );
    }

    #[test]
    fn pure_deletion_in_middle() {
        let ops = myers_diff(&[1, 2, 3], &[1, 3]);
        assert_eq!(
            ops,
            vec![DiffOp::Keep(1), DiffOp::Delete(2), DiffOp::Keep(3)]
        );
    }

    #[test]
    fn fully_disjoint_sequences() {
        // No tokens in common: all deletes then all inserts (Myers
        // tie-breaking puts deletes first at this edit distance).
        let ops = myers_diff(&[1, 2], &[3, 4]);
        assert_eq!(ops.iter().filter(|op| matches!(op, DiffOp::Keep(_))).count(), 0);
        // We get exactly 4 ops in some Delete/Insert ordering.
        assert_eq!(ops.len(), 4);
        let dels = ops.iter().filter(|op| matches!(op, DiffOp::Delete(_))).count();
        let ins = ops.iter().filter(|op| matches!(op, DiffOp::Insert(_))).count();
        assert_eq!(dels, 2);
        assert_eq!(ins, 2);
    }

    #[test]
    fn duplicate_tokens_are_matched_in_order() {
        // a = [1, 2, 1], b = [1, 1]: drop one of the 1's and the 2.
        // Myers will pick *some* valid alignment; we just check that
        // the transcript "applies" to produce b.
        let ops = myers_diff(&[1, 2, 1], &[1, 1]);
        let reconstructed: Vec<u32> = ops
            .iter()
            .filter_map(|op| match op {
                DiffOp::Keep(v) | DiffOp::Insert(v) => Some(*v),
                DiffOp::Delete(_) => None,
            })
            .collect();
        assert_eq!(reconstructed, vec![1, 1]);
    }

    #[test]
    fn transcript_applies_to_yield_curr() {
        // General invariant: applying Keeps + Inserts in order
        // reconstructs `b`; Keeps + Deletes in order reconstructs `a`.
        let cases: &[(&[u32], &[u32])] = &[
            (&[], &[1, 2]),
            (&[1, 2], &[]),
            (&[1, 2, 3, 4, 5], &[1, 2, 3, 4, 5]),
            (&[1, 2, 3], &[3, 2, 1]),
            (&[10, 20, 30, 40], &[20, 30, 50]),
            (&[1, 2, 1, 2, 1], &[2, 1, 2, 1, 2]),
        ];
        for (a, b) in cases {
            let ops = myers_diff(a, b);
            let curr: Vec<u32> = ops
                .iter()
                .filter_map(|op| match op {
                    DiffOp::Keep(v) | DiffOp::Insert(v) => Some(*v),
                    DiffOp::Delete(_) => None,
                })
                .collect();
            let prev: Vec<u32> = ops
                .iter()
                .filter_map(|op| match op {
                    DiffOp::Keep(v) | DiffOp::Delete(v) => Some(*v),
                    DiffOp::Insert(_) => None,
                })
                .collect();
            assert_eq!(curr, b.to_vec(), "applying transcript should yield b; case {a:?} -> {b:?}");
            assert_eq!(prev, a.to_vec(), "reverse-applying transcript should yield a; case {a:?} -> {b:?}");
        }
    }

    #[test]
    fn diff_is_minimal_edit_distance() {
        // a → b can be done with d = 2 (one delete + one insert).
        // Total ops in the transcript = keeps + d.
        let ops = myers_diff(&[1, 2, 3], &[1, 4, 3]);
        let keeps = ops.iter().filter(|op| matches!(op, DiffOp::Keep(_))).count();
        let edits = ops.len() - keeps;
        assert_eq!(edits, 2, "{ops:?}");
    }

    #[test]
    fn intern_assigns_shared_ids_across_sequences() {
        let a = vec!["foo".to_string(), "bar".to_string(), "baz".to_string()];
        let b = vec!["bar".to_string(), "qux".to_string()];
        let (ia, ib, values) = intern_sequences(&a, &b);
        // foo / bar / baz / qux all distinct.
        assert_eq!(values.len(), 4);
        // bar in a and in b shares its id.
        assert_eq!(ia[1], ib[0]);
        // distinct strings have distinct ids.
        assert_ne!(ia[0], ia[1]);
        assert_ne!(ib[0], ib[1]);
    }

    #[test]
    fn intern_then_diff_round_trips_through_strings() {
        let a: Vec<String> = ["hello", "world", "foo"].iter().map(|s| s.to_string()).collect();
        let b: Vec<String> = ["hello", "bar", "foo"].iter().map(|s| s.to_string()).collect();
        let (ia, ib, values) = intern_sequences(&a, &b);
        let ops = myers_diff(&ia, &ib);

        // We expect: keep hello, delete world, insert bar, keep foo.
        let as_strings: Vec<(&str, &str)> = ops
            .iter()
            .map(|op| match op {
                DiffOp::Keep(v) => ("keep", values[*v as usize].as_str()),
                DiffOp::Delete(v) => ("delete", values[*v as usize].as_str()),
                DiffOp::Insert(v) => ("insert", values[*v as usize].as_str()),
            })
            .collect();
        assert_eq!(
            as_strings,
            vec![
                ("keep", "hello"),
                ("delete", "world"),
                ("insert", "bar"),
                ("keep", "foo"),
            ]
        );
    }

    #[test]
    fn long_sequence_with_small_edit_distance() {
        // 200-token sequences differing by 2 tokens. Exercises the V
        // array sizing without being a perf benchmark.
        let a: Vec<u32> = (0..200).collect();
        let mut b = a.clone();
        b[50] = 999;
        b[150] = 1000;
        let ops = myers_diff(&a, &b);
        let keeps = ops.iter().filter(|op| matches!(op, DiffOp::Keep(_))).count();
        assert_eq!(keeps, 198);
        // Reconstruct b from keep+insert ops.
        let reconstructed: Vec<u32> = ops
            .iter()
            .filter_map(|op| match op {
                DiffOp::Keep(v) | DiffOp::Insert(v) => Some(*v),
                DiffOp::Delete(_) => None,
            })
            .collect();
        assert_eq!(reconstructed, b);
        // And we can reconstruct a.
        let reconstructed_a: Vec<u32> = ops
            .iter()
            .filter_map(|op| match op {
                DiffOp::Keep(v) | DiffOp::Delete(v) => Some(*v),
                DiffOp::Insert(_) => None,
            })
            .collect();
        a.iter().zip(reconstructed_a.iter()).for_each(|(x, y)| assert_eq!(x, y));
    }
}
