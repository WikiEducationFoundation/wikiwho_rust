//! Token-level authorship attribution for Wikipedia revisions.
//!
//! Port of the algorithm in
//! `../wikiwho_api/lib/WikiWho/WikiWho/wikiwho.py`. Parity with that
//! reference implementation is the load-bearing correctness constraint;
//! see `../../ALGORITHM.md` for the full spec and `../../CLAUDE.md` for
//! the autonomy posture.
//!
//! The current state of this crate is **tokenizer only** — the
//! attribution algorithm itself is not yet implemented. The
//! `tokenize` module reproduces `WikiWho/utils.py` byte-for-byte.

pub mod tokenize;
