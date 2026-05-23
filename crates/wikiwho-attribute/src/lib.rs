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
//!   `MatchedSets` replaces it).
//! - `spam` — constants + length-shrink + hash-match vandalism
//!   checks. The token-density check is wired in by `cascade`.
//! - `diff` — Myers diff over interned `&[u32]` token id sequences.
//!   Replaces Python `difflib.Differ` in the general token cascade.
//! - `cascade` — full paragraph + sentence + token cascade, including
//!   the Myers-driven general-case token diff and the post-cascade
//!   inbound/outbound recorder. `determine_authorship` is the
//!   orchestrator.
//! - `pipeline` — `Article::analyse_revision` wires the per-revision
//!   spam checks, cascade, and recorder into a single entry point.
//! - `response` — wire-format builders (API.md §1-6). Converts in-memory
//!   `Article` state into the rev_content JSON shape downstream
//!   consumers expect. Lives here (not in a server crate) because the
//!   shape is pure algorithm-output, no HTTP.

pub mod cascade;
pub mod diff;
pub mod differ;
pub mod pipeline;
pub mod response;
pub mod spam;
pub mod structures;
pub mod tokenize;
