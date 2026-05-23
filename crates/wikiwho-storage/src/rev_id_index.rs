//! `rev_id_index.bin` — per-language `rev_id → page_id` sidecar.
//!
//! Endpoint 1 from `API.md` (`/{lang}/api/v1.0.0-beta/rev_content/
//! rev_id/{rev_id}/`) needs to resolve a rev_id to a page_id without
//! a directory scan. The Python service relies on Postgres for this;
//! the rewrite carries an explicit on-disk index per language.
//!
//! Layout decision in `notes/decisions-needed.md`
//! (2026-05-23 — "rev_id → page_id index for endpoint 1"). Resolved as
//! **option A** — per-language sidecar, populated by the writer.
//!
//! The file lives at `<volume>/<language>/rev_id_index.bin`. It is a
//! flat sorted array of `(rev_id, page_id)` pairs with a small
//! validated wrapper. Lookups binary-search on rev_id; writes
//! wholesale-rewrite via tmp-file + rename. Append-log is a
//! follow-up (mirrors the Strategy B trajectory in `STORAGE.md §4`).
//!
//! Layout:
//!
//! ```text
//! header (24 bytes):
//!   "WRIX"           magic
//!   u16 BE           version = 1
//!   u16 BE           reserved
//!   u64 BE           n_entries
//!   u64 BE           reserved (for future delta-log offset)
//!
//! body (16 × n_entries bytes, sorted ascending by rev_id):
//!   u64 BE           rev_id
//!   u64 BE           page_id
//!
//! trailer (8 bytes):
//!   "XIRW"           magic
//!   u32 BE           CRC32 of preceding bytes
//! ```
//!
//! Fixed-width entries (not varints) are deliberate — binary search
//! needs O(1) random access. At 16 bytes per entry, en's ~700 M
//! revisions would be ~11 GB on disk. Tractable as a sidecar; if it
//! ever becomes a bottleneck we'll memory-map and revisit.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::codec::{
    crc32, read_u16_be, read_u32_be, read_u64_be, write_u16_be, write_u64_be,
};
use crate::{Result, SCHEMA_VERSION, StorageError};

pub const MAGIC_HEAD: &[u8; 4] = b"WRIX";
pub const MAGIC_TAIL: &[u8; 4] = b"XIRW";
pub const FILE_NAME: &str = "rev_id_index.bin";
const FILE_LABEL: &str = "rev_id_index.bin";

const HEADER_SIZE: usize = 24;
const TRAILER_SIZE: usize = 8;
const ENTRY_SIZE: usize = 16;

/// In-memory representation of the index. Entries are kept sorted by
/// `rev_id` so binary search is O(log N).
#[derive(Debug, Default, Clone)]
pub struct RevIdIndex {
    /// `(rev_id, page_id)` pairs, sorted ascending by rev_id.
    entries: Vec<(u64, u64)>,
}

impl RevIdIndex {
    /// Empty index — same as the on-disk state when no file exists yet.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load the per-language index from disk. Returns an empty index if
    /// the file does not exist (a fresh language, or one that pre-dates
    /// this sidecar).
    pub fn load(volume: &Path, language: &str) -> Result<Self> {
        let path = index_path(volume, language);
        match fs::read(&path) {
            Ok(bytes) => parse(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::empty()),
            Err(e) => Err(StorageError::Io(e)),
        }
    }

    /// Resolve a `rev_id` to its `page_id`, or `None` if unknown.
    pub fn lookup(&self, rev_id: u64) -> Option<u64> {
        self.entries
            .binary_search_by_key(&rev_id, |(r, _)| *r)
            .ok()
            .map(|idx| self.entries[idx].1)
    }

    /// Number of `(rev_id, page_id)` entries currently in the index.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Replace the set of rev_ids associated with `page_id`. Any prior
    /// entries with the matching `page_id` are dropped; `rev_ids` are
    /// added; the index is re-sorted. Called by the writer after a
    /// fresh `write_article`.
    ///
    /// Returns `Err(Malformed)` if a rev_id collides with a different
    /// page_id already in the index — that would mean two articles
    /// claim the same revision, which is a bug we want to surface
    /// rather than silently overwrite.
    pub fn replace_article(&mut self, page_id: u64, rev_ids: &[u64]) -> Result<()> {
        self.entries.retain(|(_, pid)| *pid != page_id);
        for &rev_id in rev_ids {
            self.entries.push((rev_id, page_id));
        }
        self.entries.sort_unstable_by_key(|(r, _)| *r);
        // After sort, adjacent duplicates with different page_ids are
        // the failure mode worth surfacing.
        for win in self.entries.windows(2) {
            if win[0].0 == win[1].0 {
                if win[0].1 == win[1].1 {
                    // Same rev_id reported twice for the same page —
                    // unusual but harmless. Caller (writer) should
                    // already have deduplicated.
                    continue;
                }
                return Err(StorageError::Malformed {
                    file: "rev_id_index.bin",
                    detail: format!(
                        "rev_id {} maps to both page_id {} and page_id {}",
                        win[0].0, win[0].1, win[1].1
                    ),
                });
            }
        }
        // Dedupe true duplicates (same rev_id, same page_id) — possible
        // if a caller passes the same rev_id twice.
        self.entries.dedup_by_key(|(r, _)| *r);
        Ok(())
    }

    /// Atomically persist the index to disk under
    /// `<volume>/<language>/rev_id_index.bin`. Uses a tmp file + rename
    /// so a partial write never replaces a good index file.
    pub fn save(&self, volume: &Path, language: &str) -> Result<()> {
        let lang_dir = volume.join(language);
        fs::create_dir_all(&lang_dir)?;
        let final_path = lang_dir.join(FILE_NAME);
        let tmp_path = lang_dir.join(format!("{FILE_NAME}.tmp"));

        let mut body = Vec::with_capacity(
            HEADER_SIZE + ENTRY_SIZE * self.entries.len() + TRAILER_SIZE,
        );
        body.extend_from_slice(MAGIC_HEAD);
        write_u16_be(&mut body, SCHEMA_VERSION)?;
        write_u16_be(&mut body, 0)?;
        let n_entries = u64::try_from(self.entries.len()).map_err(|_| {
            StorageError::Malformed {
                file: "rev_id_index.bin",
                detail: format!("too many entries ({})", self.entries.len()),
            }
        })?;
        write_u64_be(&mut body, n_entries)?;
        write_u64_be(&mut body, 0)?; // reserved
        for (rev_id, page_id) in &self.entries {
            write_u64_be(&mut body, *rev_id)?;
            write_u64_be(&mut body, *page_id)?;
        }
        let crc = crc32(&body);
        body.extend_from_slice(MAGIC_TAIL);
        body.extend_from_slice(&crc.to_be_bytes());

        {
            let mut f = fs::File::create(&tmp_path)?;
            f.write_all(&body)?;
            f.sync_all()?;
        }
        fs::rename(&tmp_path, &final_path)?;
        Ok(())
    }

    /// Convenience: load → replace → save. Wraps the
    /// read-modify-write transaction used by the writer.
    pub fn update_for_article(
        volume: &Path,
        language: &str,
        page_id: u64,
        rev_ids: &[u64],
    ) -> Result<()> {
        let mut index = Self::load(volume, language)?;
        index.replace_article(page_id, rev_ids)?;
        index.save(volume, language)?;
        Ok(())
    }

    /// Borrow the sorted `(rev_id, page_id)` entries. Mostly useful for
    /// tests; production code should call [`Self::lookup`].
    pub fn entries(&self) -> &[(u64, u64)] {
        &self.entries
    }
}

/// Compute the on-disk path. Public so the admin rebuild binary can
/// stat it.
pub fn index_path(volume: &Path, language: &str) -> PathBuf {
    volume.join(language).join(FILE_NAME)
}

fn parse(bytes: &[u8]) -> Result<RevIdIndex> {
    if bytes.len() < HEADER_SIZE + TRAILER_SIZE {
        return Err(StorageError::UnexpectedEof { file: FILE_LABEL });
    }

    let payload_len = bytes.len() - TRAILER_SIZE;
    let (payload, trailer) = bytes.split_at(payload_len);
    if &trailer[0..4] != MAGIC_TAIL {
        return Err(StorageError::BadMagic {
            file: FILE_LABEL,
            expected: *MAGIC_TAIL,
            actual: [trailer[0], trailer[1], trailer[2], trailer[3]],
        });
    }
    let expected_crc = read_u32_be(&trailer[4..8]);
    let actual_crc = crc32(payload);
    if expected_crc != actual_crc {
        return Err(StorageError::CrcMismatch {
            file: FILE_LABEL,
            expected: expected_crc,
            actual: actual_crc,
        });
    }

    if &payload[0..4] != MAGIC_HEAD {
        return Err(StorageError::BadMagic {
            file: FILE_LABEL,
            expected: *MAGIC_HEAD,
            actual: [payload[0], payload[1], payload[2], payload[3]],
        });
    }
    let version = read_u16_be(&payload[4..6]);
    if version > SCHEMA_VERSION {
        return Err(StorageError::UnsupportedVersion {
            file: FILE_LABEL,
            got: version,
            max: SCHEMA_VERSION,
        });
    }
    // payload[6..8] reserved
    let n_entries = read_u64_be(&payload[8..16]) as usize;
    // payload[16..24] reserved
    let body_start = HEADER_SIZE;
    let body_end = body_start + n_entries * ENTRY_SIZE;
    if body_end != payload.len() {
        return Err(StorageError::Malformed {
            file: FILE_LABEL,
            detail: format!(
                "header says {n_entries} entries → {body_end} body bytes, but payload is {} bytes",
                payload.len()
            ),
        });
    }

    let mut entries = Vec::with_capacity(n_entries);
    let mut last_rev_id: Option<u64> = None;
    for i in 0..n_entries {
        let off = body_start + i * ENTRY_SIZE;
        let rev_id = read_u64_be(&payload[off..off + 8]);
        let page_id = read_u64_be(&payload[off + 8..off + 16]);
        if let Some(prev) = last_rev_id {
            if rev_id <= prev {
                return Err(StorageError::Malformed {
                    file: FILE_LABEL,
                    detail: format!(
                        "entries not strictly ascending by rev_id: {prev} >= {rev_id}"
                    ),
                });
            }
        }
        last_rev_id = Some(rev_id);
        entries.push((rev_id, page_id));
    }

    Ok(RevIdIndex { entries })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_index_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let idx = RevIdIndex::empty();
        idx.save(tmp.path(), "en").unwrap();
        let reloaded = RevIdIndex::load(tmp.path(), "en").unwrap();
        assert!(reloaded.is_empty());
        assert_eq!(reloaded.len(), 0);
    }

    #[test]
    fn load_returns_empty_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let idx = RevIdIndex::load(tmp.path(), "en").unwrap();
        assert!(idx.is_empty());
    }

    #[test]
    fn replace_article_adds_entries_sorted() {
        let mut idx = RevIdIndex::empty();
        idx.replace_article(7, &[100, 50, 200]).unwrap();
        assert_eq!(idx.len(), 3);
        let entries: Vec<_> = idx.entries().to_vec();
        assert_eq!(entries, vec![(50, 7), (100, 7), (200, 7)]);
    }

    #[test]
    fn replace_article_drops_prior_entries_for_same_page_id() {
        let mut idx = RevIdIndex::empty();
        idx.replace_article(7, &[100, 200]).unwrap();
        idx.replace_article(8, &[150]).unwrap();
        // Replace page 7's revisions; page 8's stay.
        idx.replace_article(7, &[100, 300]).unwrap();
        let entries: Vec<_> = idx.entries().to_vec();
        assert_eq!(entries, vec![(100, 7), (150, 8), (300, 7)]);
    }

    #[test]
    fn replace_article_dedupes_repeated_rev_ids_within_one_article() {
        let mut idx = RevIdIndex::empty();
        idx.replace_article(7, &[100, 100, 200, 200]).unwrap();
        let entries: Vec<_> = idx.entries().to_vec();
        assert_eq!(entries, vec![(100, 7), (200, 7)]);
    }

    #[test]
    fn replace_article_rejects_cross_page_rev_id_collision() {
        let mut idx = RevIdIndex::empty();
        idx.replace_article(7, &[100, 200]).unwrap();
        // Different article tries to claim rev_id 200 — should fail.
        let err = idx.replace_article(9, &[200, 300]).unwrap_err();
        assert!(matches!(err, StorageError::Malformed { .. }));
    }

    #[test]
    fn lookup_finds_existing_rev_id() {
        let mut idx = RevIdIndex::empty();
        idx.replace_article(7, &[100, 200, 300]).unwrap();
        idx.replace_article(8, &[150]).unwrap();
        assert_eq!(idx.lookup(100), Some(7));
        assert_eq!(idx.lookup(200), Some(7));
        assert_eq!(idx.lookup(300), Some(7));
        assert_eq!(idx.lookup(150), Some(8));
    }

    #[test]
    fn lookup_returns_none_for_unknown_rev_id() {
        let mut idx = RevIdIndex::empty();
        idx.replace_article(7, &[100, 200]).unwrap();
        assert_eq!(idx.lookup(150), None);
        assert_eq!(idx.lookup(0), None);
        assert_eq!(idx.lookup(u64::MAX), None);
    }

    #[test]
    fn save_then_load_round_trips_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let mut idx = RevIdIndex::empty();
        idx.replace_article(7, &[100, 200]).unwrap();
        idx.replace_article(42, &[150, 250, 9999]).unwrap();
        idx.save(tmp.path(), "en").unwrap();

        let reloaded = RevIdIndex::load(tmp.path(), "en").unwrap();
        assert_eq!(reloaded.len(), 5);
        assert_eq!(reloaded.lookup(100), Some(7));
        assert_eq!(reloaded.lookup(150), Some(42));
        assert_eq!(reloaded.lookup(9999), Some(42));
    }

    #[test]
    fn update_for_article_is_load_replace_save() {
        let tmp = tempfile::tempdir().unwrap();
        RevIdIndex::update_for_article(tmp.path(), "en", 7, &[100, 200]).unwrap();
        RevIdIndex::update_for_article(tmp.path(), "en", 8, &[150]).unwrap();
        RevIdIndex::update_for_article(tmp.path(), "en", 7, &[100, 300]).unwrap();

        let idx = RevIdIndex::load(tmp.path(), "en").unwrap();
        assert_eq!(idx.lookup(100), Some(7));
        assert_eq!(idx.lookup(150), Some(8));
        assert_eq!(idx.lookup(300), Some(7));
        assert_eq!(idx.lookup(200), None); // dropped by the third update
    }

    #[test]
    fn save_uses_atomic_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let mut idx = RevIdIndex::empty();
        idx.replace_article(7, &[100, 200]).unwrap();
        idx.save(tmp.path(), "en").unwrap();
        // Tmp file should not linger after a successful save.
        let tmp_marker = tmp.path().join("en").join(format!("{FILE_NAME}.tmp"));
        assert!(!tmp_marker.exists(), "tmp file leftover: {tmp_marker:?}");
    }

    #[test]
    fn parse_rejects_bad_head_magic() {
        let tmp = tempfile::tempdir().unwrap();
        let mut idx = RevIdIndex::empty();
        idx.replace_article(7, &[100]).unwrap();
        idx.save(tmp.path(), "en").unwrap();
        let path = index_path(tmp.path(), "en");
        let mut bytes = fs::read(&path).unwrap();
        bytes[0] = b'!';
        fs::write(&path, &bytes).unwrap();
        let err = RevIdIndex::load(tmp.path(), "en").unwrap_err();
        assert!(matches!(
            err,
            StorageError::BadMagic { .. } | StorageError::CrcMismatch { .. }
        ));
    }

    #[test]
    fn parse_rejects_bad_tail_magic() {
        let tmp = tempfile::tempdir().unwrap();
        let mut idx = RevIdIndex::empty();
        idx.replace_article(7, &[100]).unwrap();
        idx.save(tmp.path(), "en").unwrap();
        let path = index_path(tmp.path(), "en");
        let mut bytes = fs::read(&path).unwrap();
        let len = bytes.len();
        bytes[len - 8] = b'!';
        fs::write(&path, &bytes).unwrap();
        let err = RevIdIndex::load(tmp.path(), "en").unwrap_err();
        assert!(matches!(err, StorageError::BadMagic { .. }));
    }

    #[test]
    fn parse_rejects_crc_corruption() {
        let tmp = tempfile::tempdir().unwrap();
        let mut idx = RevIdIndex::empty();
        idx.replace_article(7, &[100, 200, 300]).unwrap();
        idx.save(tmp.path(), "en").unwrap();
        let path = index_path(tmp.path(), "en");
        let mut bytes = fs::read(&path).unwrap();
        // Flip a byte inside the body.
        bytes[HEADER_SIZE + 5] ^= 0x40;
        fs::write(&path, &bytes).unwrap();
        let err = RevIdIndex::load(tmp.path(), "en").unwrap_err();
        assert!(matches!(err, StorageError::CrcMismatch { .. }));
    }

    #[test]
    fn parse_rejects_unsorted_entries() {
        // Construct a deliberately-unsorted file and verify load() rejects.
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("en")).unwrap();
        let path = index_path(tmp.path(), "en");

        let mut body = Vec::new();
        body.extend_from_slice(MAGIC_HEAD);
        write_u16_be(&mut body, SCHEMA_VERSION).unwrap();
        write_u16_be(&mut body, 0).unwrap();
        write_u64_be(&mut body, 2).unwrap();
        write_u64_be(&mut body, 0).unwrap();
        // Entries in wrong order.
        write_u64_be(&mut body, 500).unwrap();
        write_u64_be(&mut body, 7).unwrap();
        write_u64_be(&mut body, 100).unwrap();
        write_u64_be(&mut body, 9).unwrap();
        let crc = crc32(&body);
        body.extend_from_slice(MAGIC_TAIL);
        body.extend_from_slice(&crc.to_be_bytes());
        fs::write(&path, &body).unwrap();

        let err = RevIdIndex::load(tmp.path(), "en").unwrap_err();
        assert!(matches!(err, StorageError::Malformed { .. }));
    }

    #[test]
    fn parse_rejects_truncated_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut idx = RevIdIndex::empty();
        idx.replace_article(7, &[100, 200]).unwrap();
        idx.save(tmp.path(), "en").unwrap();
        let path = index_path(tmp.path(), "en");
        let bytes = fs::read(&path).unwrap();
        // Drop the last 4 bytes — corrupts the trailer.
        fs::write(&path, &bytes[..bytes.len() - 4]).unwrap();
        let err = RevIdIndex::load(tmp.path(), "en").unwrap_err();
        // Either CrcMismatch (trailer shifted into payload) or BadMagic
        // (truncation moved the magic) is acceptable; we must NOT return
        // success on a truncated file.
        assert!(matches!(
            err,
            StorageError::BadMagic { .. }
                | StorageError::CrcMismatch { .. }
                | StorageError::UnexpectedEof { .. }
        ));
    }

    #[test]
    fn save_is_idempotent_on_unchanged_index() {
        let tmp = tempfile::tempdir().unwrap();
        let mut idx = RevIdIndex::empty();
        idx.replace_article(7, &[100, 200]).unwrap();
        idx.save(tmp.path(), "en").unwrap();
        let path = index_path(tmp.path(), "en");
        let first = fs::read(&path).unwrap();
        idx.save(tmp.path(), "en").unwrap();
        let second = fs::read(&path).unwrap();
        assert_eq!(first, second, "rewriting an unchanged index should be byte-identical");
    }
}
