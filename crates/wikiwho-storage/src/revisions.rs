//! `revisions.bin` — per-revision metadata + token sequences (STORAGE.md §2.4).
//!
//! This is the file the `rev_content` request path reads from. The
//! revision-id index table at the head allows O(log N) binary search
//! by rev_id; each record then sits at a single mmap offset.
//!
//! Spec deviations from STORAGE.md §2.4, all documented inline:
//!
//! - **Timestamp is stored as a length-prefixed UTF-8 string**, not as
//!   an i64 unix seconds. The wire format echoes the MW-API string
//!   byte-for-byte (`API.md §1`); persisting the original avoids a
//!   round-trip parse/format pair and any risk of drift. Cost is ~12
//!   extra bytes per revision (~1.5 % file-size hit on representative
//!   fixtures); worth the simplicity at first cut.
//! - **Editor is stored as a length-prefixed UTF-8 string** rather than
//!   `(editor_kind, editor_id|string_id)`. Same rationale — the
//!   wire-format consumer needs the string form directly.
//! - **No `parent_rev_id`.** The algorithm carries `last_good_rev_id`
//!   internally, but `rev_content` doesn't expose it, and reconstructing
//!   the previous-processed-revision pointer at read time isn't needed
//!   for the read path. If we ever extend the algorithm to resume
//!   processing from disk we'll fold this into `appendlog.bin` or a
//!   separate header.
//! - **Token sequence is stored explicitly per revision.** This is what
//!   replaces the `paragraphs → sentences → words` walk; without it
//!   the read side would need `paragraphs.bin` / `sentences.bin` files
//!   too. The cost is bounded — one varint per token, delta-encoded
//!   within the revision — and it's the read-hot path.
//!
//! Layout:
//!
//! ```text
//! header (24 bytes):
//!   "WWRV"           magic
//!   u16 BE           version = 1
//!   u16 BE           reserved
//!   u32 BE           n_revisions
//!   u32 BE           offset of revision-id index table (from start of file)
//!   u32 BE           byte size of revision data section
//!   u32 BE           reserved
//!
//! revision data section (variable, ascending storage order):
//!   for each revision:
//!     varint u64:    rev_id (absolute)
//!     varint u64:    timestamp length, then UTF-8 bytes
//!     varint u64:    editor length, then UTF-8 bytes
//!     varint u64:    n_tokens
//!     varint zigzag × n_tokens: token ids, delta-encoded within rev
//!                    (first token: delta from 0, subsequent: from prev)
//!
//! revision-id index table (12 × n_revisions bytes, sorted by rev_id):
//!     u64 BE         rev_id
//!     u32 BE         offset into revision data section
//!
//! trailer (8 bytes):
//!   "VRWW"           magic
//!   u32 BE           CRC32 of preceding bytes
//! ```
//!
//! Note: the index table appears **after** the data section in the
//! file. This keeps the write path single-pass — we don't know the
//! data section size until we've written it.

use std::io::Write;

use crate::codec::{
    crc32, read_u16_be, read_u32_be, read_u64_be, read_varint_i64, read_varint_u64, write_u16_be,
    write_u32_be, write_u64_be, write_varint_i64, write_varint_u64,
};
use crate::{Result, SCHEMA_VERSION, StorageError};

pub const MAGIC_HEAD: &[u8; 4] = b"WWRV";
pub const MAGIC_TAIL: &[u8; 4] = b"VRWW";
const FILE_NAME: &str = "revisions.bin";

/// In-memory shape of one persisted revision. `token_sequence` is the
/// flat ordered list of token ids in the revision, as the wire format
/// emits them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRevision {
    pub rev_id: u64,
    pub timestamp: String,
    pub editor: String,
    pub token_sequence: Vec<u32>,
}

/// Write the revision section + index table.
///
/// `revisions` should be in **processing order** (the order the
/// algorithm produced them, matching `Article::ordered_revisions`).
/// The index table is sorted by rev_id at write time for binary search.
pub fn write_revisions<W: Write>(w: &mut W, revisions: &[StoredRevision]) -> Result<()> {
    let n_revisions = u32::try_from(revisions.len()).map_err(|_| StorageError::Malformed {
        file: FILE_NAME,
        detail: format!("too many revisions ({})", revisions.len()),
    })?;

    // First pass: encode each revision's body, recording the start
    // offset (into the data section) for the index table.
    let mut data_section: Vec<u8> = Vec::new();
    let mut offsets: Vec<(u64, u32)> = Vec::with_capacity(revisions.len());

    for rev in revisions {
        let offset = u32::try_from(data_section.len()).map_err(|_| StorageError::Malformed {
            file: FILE_NAME,
            detail: "data section offset overflowed u32".to_string(),
        })?;
        offsets.push((rev.rev_id, offset));

        write_varint_u64(&mut data_section, rev.rev_id)?;
        write_varint_u64(&mut data_section, rev.timestamp.len() as u64)?;
        data_section.extend_from_slice(rev.timestamp.as_bytes());
        write_varint_u64(&mut data_section, rev.editor.len() as u64)?;
        data_section.extend_from_slice(rev.editor.as_bytes());

        write_varint_u64(&mut data_section, rev.token_sequence.len() as u64)?;
        let mut prev: i64 = 0;
        for &tid in &rev.token_sequence {
            let delta = tid as i64 - prev;
            write_varint_i64(&mut data_section, delta)?;
            prev = tid as i64;
        }
    }

    let data_size = u32::try_from(data_section.len()).map_err(|_| StorageError::Malformed {
        file: FILE_NAME,
        detail: "data section size overflowed u32".to_string(),
    })?;

    // Sort index entries by rev_id for binary search.
    offsets.sort_unstable_by_key(|(id, _)| *id);

    // Header (24 bytes) + data + index table.
    let header_len = 24u32;
    let index_offset = header_len + data_size;

    let mut body: Vec<u8> = Vec::with_capacity(
        header_len as usize + data_section.len() + 12 * revisions.len(),
    );
    body.extend_from_slice(MAGIC_HEAD);
    write_u16_be(&mut body, SCHEMA_VERSION)?;
    write_u16_be(&mut body, 0)?;
    write_u32_be(&mut body, n_revisions)?;
    write_u32_be(&mut body, index_offset)?;
    write_u32_be(&mut body, data_size)?;
    write_u32_be(&mut body, 0)?;

    body.extend_from_slice(&data_section);
    for (rev_id, offset) in &offsets {
        write_u64_be(&mut body, *rev_id)?;
        write_u32_be(&mut body, *offset)?;
    }

    let crc = crc32(&body);
    body.extend_from_slice(MAGIC_TAIL);
    body.extend_from_slice(&crc.to_be_bytes());

    w.write_all(&body)?;
    Ok(())
}

/// Sequential read of every revision in processing order. Used by the
/// round-trip path; not the read-hot path. For random access by
/// rev_id use [`RevisionsIndex`].
pub fn parse_revisions_blob(all: &[u8]) -> Result<Vec<StoredRevision>> {
    let header = read_and_validate_header(all)?;
    let data_start = 24;
    let data_end = data_start + header.data_size as usize;

    let mut cur = std::io::Cursor::new(&all[data_start..data_end]);
    let mut out = Vec::with_capacity(header.n_revisions as usize);
    for _ in 0..header.n_revisions {
        out.push(read_one_revision(&mut cur)?);
    }
    Ok(out)
}

struct ParsedHeader {
    n_revisions: u32,
    #[allow(dead_code)]
    index_offset: u32,
    data_size: u32,
}

fn read_and_validate_header(all: &[u8]) -> Result<ParsedHeader> {
    if all.len() < 24 + 8 {
        return Err(StorageError::UnexpectedEof { file: FILE_NAME });
    }
    let payload_len = all.len() - 8;
    let (payload, trailer) = all.split_at(payload_len);
    if &trailer[0..4] != MAGIC_TAIL {
        return Err(StorageError::BadMagic {
            file: FILE_NAME,
            expected: *MAGIC_TAIL,
            actual: [trailer[0], trailer[1], trailer[2], trailer[3]],
        });
    }
    let expected_crc = read_u32_be(&trailer[4..8]);
    let actual_crc = crc32(payload);
    if expected_crc != actual_crc {
        return Err(StorageError::CrcMismatch {
            file: FILE_NAME,
            expected: expected_crc,
            actual: actual_crc,
        });
    }
    if &payload[0..4] != MAGIC_HEAD {
        return Err(StorageError::BadMagic {
            file: FILE_NAME,
            expected: *MAGIC_HEAD,
            actual: [payload[0], payload[1], payload[2], payload[3]],
        });
    }
    let version = read_u16_be(&payload[4..6]);
    if version > SCHEMA_VERSION {
        return Err(StorageError::UnsupportedVersion {
            file: FILE_NAME,
            got: version,
            max: SCHEMA_VERSION,
        });
    }
    let n_revisions = read_u32_be(&payload[8..12]);
    let index_offset = read_u32_be(&payload[12..16]);
    let data_size = read_u32_be(&payload[16..20]);
    let expected_index_offset = 24 + data_size;
    if index_offset != expected_index_offset {
        return Err(StorageError::Malformed {
            file: FILE_NAME,
            detail: format!(
                "header: index_offset {index_offset} != header_len + data_size {expected_index_offset}"
            ),
        });
    }
    Ok(ParsedHeader {
        n_revisions,
        index_offset,
        data_size,
    })
}

fn read_one_revision(cur: &mut std::io::Cursor<&[u8]>) -> Result<StoredRevision> {
    let rev_id = read_varint_u64(cur, FILE_NAME)?;
    let ts_len = read_varint_u64(cur, FILE_NAME)? as usize;
    let timestamp = read_utf8(cur, ts_len, "timestamp")?;
    let ed_len = read_varint_u64(cur, FILE_NAME)? as usize;
    let editor = read_utf8(cur, ed_len, "editor")?;
    let n_tokens = read_varint_u64(cur, FILE_NAME)? as usize;

    let mut token_sequence = Vec::with_capacity(n_tokens);
    let mut prev: i64 = 0;
    for _ in 0..n_tokens {
        let delta = read_varint_i64(cur, FILE_NAME)?;
        let v = prev + delta;
        if !(0..=u32::MAX as i64).contains(&v) {
            return Err(StorageError::Malformed {
                file: FILE_NAME,
                detail: format!("token_id out of u32 range: {v}"),
            });
        }
        token_sequence.push(v as u32);
        prev = v;
    }

    Ok(StoredRevision {
        rev_id,
        timestamp,
        editor,
        token_sequence,
    })
}

fn read_utf8(cur: &mut std::io::Cursor<&[u8]>, n: usize, field: &'static str) -> Result<String> {
    let pos = cur.position() as usize;
    let inner = *cur.get_ref();
    if pos + n > inner.len() {
        return Err(StorageError::UnexpectedEof { file: FILE_NAME });
    }
    let slice = &inner[pos..pos + n];
    cur.set_position((pos + n) as u64);
    std::str::from_utf8(slice)
        .map(|s| s.to_string())
        .map_err(|e| StorageError::Malformed {
            file: FILE_NAME,
            detail: format!("invalid utf8 in {field}: {e}"),
        })
}

/// Random-access read of `revisions.bin`. Constructed once over a
/// mmap'd payload; each call to [`Self::get`] does a binary search +
/// single varint stream decode.
pub struct RevisionsIndex<'a> {
    payload: &'a [u8],
    index_start: usize,
    data_start: usize,
    n_revisions: usize,
}

impl<'a> RevisionsIndex<'a> {
    pub fn new(all: &'a [u8]) -> Result<Self> {
        let header = read_and_validate_header(all)?;
        let payload_len = all.len() - 8;
        let payload = &all[..payload_len];
        let data_start = 24;
        let index_start = data_start + header.data_size as usize;
        Ok(Self {
            payload,
            index_start,
            data_start,
            n_revisions: header.n_revisions as usize,
        })
    }

    pub fn len(&self) -> usize {
        self.n_revisions
    }

    pub fn is_empty(&self) -> bool {
        self.n_revisions == 0
    }

    /// Look up a revision by id. Returns `Ok(None)` if not present.
    pub fn get(&self, rev_id: u64) -> Result<Option<StoredRevision>> {
        let entry_size = 12usize;
        let mut lo = 0usize;
        let mut hi = self.n_revisions;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let entry = self.index_start + mid * entry_size;
            let mid_rev_id = read_u64_be(&self.payload[entry..entry + 8]);
            match mid_rev_id.cmp(&rev_id) {
                std::cmp::Ordering::Equal => {
                    let offset = read_u32_be(&self.payload[entry + 8..entry + 12]) as usize;
                    let mut cur = std::io::Cursor::new(&self.payload[self.data_start + offset..]);
                    return Ok(Some(read_one_revision(&mut cur)?));
                }
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        Ok(None)
    }

    /// Iterate every revision in **rev_id ascending order** (the order
    /// the index table stores). Cheaper than `parse_revisions_blob`
    /// when you only need to traverse — no full Vec allocation.
    pub fn iter_by_rev_id(&self) -> RevisionsIndexIter<'a, '_> {
        RevisionsIndexIter { idx: self, i: 0 }
    }

    /// Cheap walk over just the rev_ids in the index table — no varint
    /// decoding of any revision body. Useful for sidecar-index rebuilds
    /// that only need `(rev_id, page_id)` pairs.
    pub fn rev_ids_sorted(&self) -> Vec<u64> {
        let entry_size = 12usize;
        let mut out = Vec::with_capacity(self.n_revisions);
        for i in 0..self.n_revisions {
            let entry = self.index_start + i * entry_size;
            out.push(read_u64_be(&self.payload[entry..entry + 8]));
        }
        out
    }
}

pub struct RevisionsIndexIter<'a, 'i> {
    idx: &'i RevisionsIndex<'a>,
    i: usize,
}

impl<'a, 'i> Iterator for RevisionsIndexIter<'a, 'i> {
    type Item = Result<StoredRevision>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.i >= self.idx.n_revisions {
            return None;
        }
        let entry = self.idx.index_start + self.i * 12;
        self.i += 1;
        let offset = read_u32_be(&self.idx.payload[entry + 8..entry + 12]) as usize;
        let mut cur = std::io::Cursor::new(&self.idx.payload[self.idx.data_start + offset..]);
        Some(read_one_revision(&mut cur))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<StoredRevision> {
        vec![
            StoredRevision {
                rev_id: 1000,
                timestamp: "2024-01-01T00:00:00Z".into(),
                editor: "42".into(),
                token_sequence: vec![0, 1, 2, 3],
            },
            StoredRevision {
                rev_id: 2000,
                timestamp: "2024-01-02T00:00:00Z".into(),
                editor: "0|192.0.2.1".into(),
                token_sequence: vec![0, 1, 5, 2, 3],
            },
            // Out-of-order in processing order but the index table
            // will sort it.
            StoredRevision {
                rev_id: 1500,
                timestamp: "2024-01-03T00:00:00Z".into(),
                editor: "".into(),
                token_sequence: vec![],
            },
        ]
    }

    #[test]
    fn round_trip_sequential() {
        let revs = sample();
        let mut buf = Vec::new();
        write_revisions(&mut buf, &revs).unwrap();
        let read_back = parse_revisions_blob(&buf).unwrap();
        assert_eq!(read_back, revs, "sequential read preserves processing order");
    }

    #[test]
    fn round_trip_empty() {
        let revs: Vec<StoredRevision> = vec![];
        let mut buf = Vec::new();
        write_revisions(&mut buf, &revs).unwrap();
        let read_back = parse_revisions_blob(&buf).unwrap();
        assert!(read_back.is_empty());
    }

    #[test]
    fn random_access_by_rev_id() {
        let revs = sample();
        let mut buf = Vec::new();
        write_revisions(&mut buf, &revs).unwrap();
        let idx = RevisionsIndex::new(&buf).unwrap();
        assert_eq!(idx.len(), 3);

        let r1 = idx.get(1000).unwrap().unwrap();
        assert_eq!(r1.rev_id, 1000);
        assert_eq!(r1.editor, "42");
        assert_eq!(r1.token_sequence, vec![0, 1, 2, 3]);

        let r2 = idx.get(2000).unwrap().unwrap();
        assert_eq!(r2.rev_id, 2000);
        assert_eq!(r2.token_sequence, vec![0, 1, 5, 2, 3]);

        let r3 = idx.get(1500).unwrap().unwrap();
        assert_eq!(r3.timestamp, "2024-01-03T00:00:00Z");

        // Not present.
        assert!(idx.get(999).unwrap().is_none());
        assert!(idx.get(99999).unwrap().is_none());
    }

    #[test]
    fn iter_by_rev_id_is_sorted_ascending() {
        let revs = sample();
        let mut buf = Vec::new();
        write_revisions(&mut buf, &revs).unwrap();
        let idx = RevisionsIndex::new(&buf).unwrap();
        let collected: Vec<u64> = idx
            .iter_by_rev_id()
            .map(|r| r.unwrap().rev_id)
            .collect();
        assert_eq!(collected, vec![1000, 1500, 2000]);
    }

    #[test]
    fn crc_corruption_detected() {
        let revs = sample();
        let mut buf = Vec::new();
        write_revisions(&mut buf, &revs).unwrap();
        let mid = buf.len() / 2;
        buf[mid] ^= 0xFF;
        let err = parse_revisions_blob(&buf).unwrap_err();
        assert!(matches!(err, StorageError::CrcMismatch { .. }));
    }

    #[test]
    fn many_revisions_round_trip() {
        let revs: Vec<StoredRevision> = (1u64..=1000)
            .map(|i| StoredRevision {
                rev_id: i * 100,
                timestamp: format!("2024-01-{:02}T00:00:{:02}Z", (i % 28) + 1, i % 60),
                editor: i.to_string(),
                token_sequence: (0u32..(i as u32 % 50)).collect(),
            })
            .collect();
        let mut buf = Vec::new();
        write_revisions(&mut buf, &revs).unwrap();
        let read_back = parse_revisions_blob(&buf).unwrap();
        assert_eq!(read_back.len(), 1000);
        assert_eq!(read_back, revs);

        // Spot-check binary search at extremes.
        let idx = RevisionsIndex::new(&buf).unwrap();
        assert_eq!(idx.get(100).unwrap().unwrap().rev_id, 100);
        assert_eq!(idx.get(100_000).unwrap().unwrap().rev_id, 100_000);
        assert!(idx.get(150).unwrap().is_none()); // between entries
    }
}
