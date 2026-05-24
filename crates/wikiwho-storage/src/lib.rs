//! On-disk format for per-article authorship state.
//!
//! Implements the layout specified in `../../STORAGE.md`. The format
//! supports:
//!
//! - **Lazy single-revision reads** — answering a `rev_content` query
//!   without reading the full article into memory (via the binary
//!   search index in `revisions.bin`).
//! - **Whole-article rebuilds** — round-tripping an
//!   [`wikiwho_attribute::structures::Article`] through disk and back
//!   to in-memory state that can serve the same wire-format response.
//! - **Future append-and-compact** — `appendlog.bin` and the
//!   delta-log optimization are deferred per the resolved
//!   wholesale-rewrite strategy (STORAGE.md §4 Strategy B). The
//!   on-disk header carries a `schema_version` so we can layer that
//!   in without a format break.
//!
//! The format carries two parallel views of per-revision state:
//!
//! - **Flat token sequence** in `revisions.bin` — what the read-hot
//!   `rev_content` path walks. Cheap and cache-friendly.
//! - **Paragraph references** in `revisions.bin` + paragraph and
//!   sentence arenas in `paragraphs.bin` / `sentences.bin` + full
//!   hash-table back-refs in `hashtables.bin` — what the algorithm's
//!   resume-from-disk path walks when applying a new revision on top
//!   of a loaded `Article`.
//!
//! Per `CLAUDE.md` the load-bearing test of this crate is the
//! round-trip parity test: feed a captured-history fixture through
//! the algorithm, persist via [`writer::write_article`], reload via
//! [`reader::SnapshotReader`], and verify the resulting
//! `rev_content` response is byte-identical to the in-memory one.
//! A second integration test (`resume_from_disk_*`) extends this to
//! cover the live-update story: load → apply more revisions → match
//! a single end-to-end in-memory replay.

pub mod codec;
pub mod hashtables;
pub mod layout;
pub mod meta;
pub mod paragraphs;
pub mod reader;
pub mod rebuild;
pub mod rev_id_index;
pub mod revisions;
pub mod sentences;
pub mod strings;
pub mod tokens;
pub mod writer;

/// On-disk schema version. Incremented when a binary file's layout
/// changes in a way that older readers cannot handle. Bumped to `2`
/// when `paragraphs.bin` + `sentences.bin` landed and
/// `hashtables.bin` + `revisions.bin` grew arena-back-reference
/// payloads. No persistent production data has been written at any
/// earlier version, so the bump is purely tracking the format growth.
pub const SCHEMA_VERSION: u16 = 2;

/// Errors that can occur reading or writing the on-disk format.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json (de)serialization: {0}")]
    Json(#[from] serde_json::Error),

    #[error("bad magic in {file}: expected {expected:?}, got {actual:?}")]
    BadMagic {
        file: &'static str,
        expected: [u8; 4],
        actual: [u8; 4],
    },

    #[error("unsupported {file} format version {got}; this build understands up to {max}")]
    UnsupportedVersion {
        file: &'static str,
        got: u16,
        max: u16,
    },

    #[error("crc mismatch in {file}: expected 0x{expected:08x}, got 0x{actual:08x}")]
    CrcMismatch {
        file: &'static str,
        expected: u32,
        actual: u32,
    },

    #[error("unexpected eof reading {file}")]
    UnexpectedEof { file: &'static str },

    #[error("malformed {file}: {detail}")]
    Malformed {
        file: &'static str,
        detail: String,
    },
}

pub type Result<T> = std::result::Result<T, StorageError>;
