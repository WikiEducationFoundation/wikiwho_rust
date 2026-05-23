//! Token-level authorship attribution for Wikipedia revisions.
//!
//! Port of the algorithm in
//! `../wikiwho_api/lib/WikiWho/WikiWho/wikiwho.py`. Parity with that
//! reference implementation is the load-bearing correctness constraint;
//! see `../../ALGORITHM.md` for the full spec and `../../CLAUDE.md` for
//! the autonomy posture.
//!
//! State as of this commit:
//! - `tokenize` — full port of `WikiWho/utils.py`. Verified at 90% on
//!   real production fixtures (`scripts/verify_tokenizer.py` +
//!   `crates/wikiwho-parity`).
//! - `structures` — data types (`Word`, `Sentence`, `Paragraph`,
//!   `Revision`, `Article`) using arena-allocated indices instead of
//!   shared references; no `matched` flag on nodes (per-iteration
//!   `MatchedSets` will replace it).
//! - `spam` — constants + length-shrink + hash-match vandalism
//!   checks. The token-density check is wired in once the cascade
//!   exists.
//! - The matching cascade (`determine_authorship` and its
//!   sub-functions) is NOT YET PORTED.

pub mod spam;
pub mod structures;
pub mod tokenize;
