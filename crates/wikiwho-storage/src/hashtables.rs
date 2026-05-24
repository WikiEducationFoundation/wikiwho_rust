//! `hashtables.bin` — cross-revision paragraph + sentence hash tables
//! (STORAGE.md §4, Strategy B).
//!
//! Each hash maps to the **full list of arena ids** that share that
//! hash. The lifetime contract is that
//! `paragraphs_ht[hash] == [paragraph_ids that ever had this hash]`
//! and likewise for sentences — see
//! `wikiwho_attribute::structures::Article::paragraphs_ht`. Loading
//! this file is what enables the algorithm's resume-from-disk path:
//! when a new revision's paragraph hashes match here, the matching
//! [`crate::paragraphs::StoredParagraph`] is fetched by arena id and
//! its sentences walked the same way the algorithm walks the
//! previously-processed in-memory revision.
//!
//! Layout:
//!
//! ```text
//! header (16 bytes):
//!   "WWHT"           magic
//!   u16 BE           version
//!   u16 BE           reserved
//!   u32 BE           n_paragraph_entries
//!   u32 BE           n_sentence_entries
//!
//! paragraph entries (sorted by hash for deterministic writes):
//!   varint u64:      hash length
//!   raw bytes:       hash (UTF-8)
//!   varint u64:      bucket length
//!   varint i64:      paragraph_id delta (zigzag, from prev — first from 0)
//!                    repeated bucket-length times
//!
//! sentence entries: same shape, with `sentence_id` instead.
//!
//! trailer (8 bytes):
//!   "THWW"           magic
//!   u32 BE           CRC32 of preceding bytes
//! ```
//!
//! Arena-id deltas are signed: a hash whose bucket touches arena ids
//! `[100, 5, 200]` (a real shape for paragraphs that get reintroduced
//! after deletion) needs backward jumps. Mirrors the rev-id-delta
//! choice in `tokens.bin` for the same reason.

use std::io::Write;

use crate::codec::{
    crc32, read_u16_be, read_u32_be, read_varint_i64, read_varint_u64, write_u16_be, write_u32_be,
    write_varint_i64, write_varint_u64,
};
use crate::{Result, SCHEMA_VERSION, StorageError};

pub const MAGIC_HEAD: &[u8; 4] = b"WWHT";
pub const MAGIC_TAIL: &[u8; 4] = b"THWW";
const FILE_NAME: &str = "hashtables.bin";

/// In-memory representation of `hashtables.bin`. The two entry lists
/// are kept sorted by hash for deterministic writes; arena ids inside
/// each bucket appear in insertion order (which mirrors
/// `Article::paragraphs_ht` semantics).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HashTables {
    pub paragraph_buckets: Vec<HashBucket>,
    pub sentence_buckets: Vec<HashBucket>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashBucket {
    pub hash: String,
    /// Arena ids that share this hash. For `paragraph_buckets` these
    /// index into `Article::paragraphs`; for `sentence_buckets`,
    /// `Article::sentences`. Order matches the order arena ids were
    /// pushed into the bucket during the algorithm's lifetime.
    pub arena_ids: Vec<u32>,
}

pub fn write_hashtables<W: Write>(w: &mut W, tables: &HashTables) -> Result<()> {
    let n_p = u32::try_from(tables.paragraph_buckets.len()).map_err(|_| {
        StorageError::Malformed {
            file: FILE_NAME,
            detail: "too many paragraph hashes".to_string(),
        }
    })?;
    let n_s = u32::try_from(tables.sentence_buckets.len()).map_err(|_| {
        StorageError::Malformed {
            file: FILE_NAME,
            detail: "too many sentence hashes".to_string(),
        }
    })?;

    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(MAGIC_HEAD);
    write_u16_be(&mut body, SCHEMA_VERSION)?;
    write_u16_be(&mut body, 0)?;
    write_u32_be(&mut body, n_p)?;
    write_u32_be(&mut body, n_s)?;

    for b in &tables.paragraph_buckets {
        write_bucket(&mut body, b)?;
    }
    for b in &tables.sentence_buckets {
        write_bucket(&mut body, b)?;
    }

    let crc = crc32(&body);
    body.extend_from_slice(MAGIC_TAIL);
    body.extend_from_slice(&crc.to_be_bytes());

    w.write_all(&body)?;
    Ok(())
}

fn write_bucket(body: &mut Vec<u8>, b: &HashBucket) -> Result<()> {
    write_varint_u64(body, b.hash.len() as u64)?;
    body.extend_from_slice(b.hash.as_bytes());
    write_varint_u64(body, b.arena_ids.len() as u64)?;
    let mut prev: i64 = 0;
    for &id in &b.arena_ids {
        let delta = id as i64 - prev;
        write_varint_i64(body, delta)?;
        prev = id as i64;
    }
    Ok(())
}

pub fn parse_hashtables_blob(all: &[u8]) -> Result<HashTables> {
    if all.len() < 16 + 8 {
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
    let n_p = read_u32_be(&payload[8..12]) as usize;
    let n_s = read_u32_be(&payload[12..16]) as usize;

    let mut cur = std::io::Cursor::new(&payload[16..]);
    let mut paragraph_buckets = Vec::with_capacity(n_p);
    for _ in 0..n_p {
        paragraph_buckets.push(read_bucket(&mut cur)?);
    }
    let mut sentence_buckets = Vec::with_capacity(n_s);
    for _ in 0..n_s {
        sentence_buckets.push(read_bucket(&mut cur)?);
    }

    Ok(HashTables {
        paragraph_buckets,
        sentence_buckets,
    })
}

fn read_bucket(cur: &mut std::io::Cursor<&[u8]>) -> Result<HashBucket> {
    let hlen = read_varint_u64(cur, FILE_NAME)? as usize;
    let pos = cur.position() as usize;
    let inner = *cur.get_ref();
    if pos + hlen > inner.len() {
        return Err(StorageError::UnexpectedEof { file: FILE_NAME });
    }
    let hash = std::str::from_utf8(&inner[pos..pos + hlen])
        .map_err(|e| StorageError::Malformed {
            file: FILE_NAME,
            detail: format!("invalid utf8 hash: {e}"),
        })?
        .to_string();
    cur.set_position((pos + hlen) as u64);

    let blen = read_varint_u64(cur, FILE_NAME)? as usize;
    let mut arena_ids = Vec::with_capacity(blen);
    let mut prev: i64 = 0;
    for _ in 0..blen {
        let delta = read_varint_i64(cur, FILE_NAME)?;
        let v = prev + delta;
        if !(0..=u32::MAX as i64).contains(&v) {
            return Err(StorageError::Malformed {
                file: FILE_NAME,
                detail: format!("arena id out of u32 range: {v}"),
            });
        }
        arena_ids.push(v as u32);
        prev = v;
    }
    Ok(HashBucket { hash, arena_ids })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> HashTables {
        HashTables {
            paragraph_buckets: vec![
                HashBucket { hash: "abc123".repeat(5), arena_ids: vec![0] },
                HashBucket { hash: "deadbeef".repeat(4), arena_ids: vec![1, 2, 3] },
            ],
            sentence_buckets: vec![
                HashBucket { hash: "ffeeddcc".repeat(4), arena_ids: vec![0] },
                HashBucket {
                    hash: "11223344".repeat(4),
                    arena_ids: vec![1, 2, 3, 4, 5, 6, 7],
                },
                HashBucket { hash: "55667788".repeat(4), arena_ids: vec![8] },
            ],
        }
    }

    #[test]
    fn round_trip() {
        let h = sample();
        let mut buf = Vec::new();
        write_hashtables(&mut buf, &h).unwrap();
        let read_back = parse_hashtables_blob(&buf).unwrap();
        assert_eq!(read_back, h);
    }

    #[test]
    fn round_trip_empty() {
        let h = HashTables::default();
        let mut buf = Vec::new();
        write_hashtables(&mut buf, &h).unwrap();
        let read_back = parse_hashtables_blob(&buf).unwrap();
        assert_eq!(read_back, h);
    }

    #[test]
    fn round_trip_non_monotonic_arena_ids() {
        // Bucket arena ids that go up, then down, then up (a paragraph
        // reintroduced after deletion gets allocated as a new arena id,
        // which can be lower than later entries).
        let h = HashTables {
            paragraph_buckets: vec![HashBucket {
                hash: "loopy".into(),
                arena_ids: vec![100, 5, 200, 0, 1500],
            }],
            sentence_buckets: vec![],
        };
        let mut buf = Vec::new();
        write_hashtables(&mut buf, &h).unwrap();
        let read_back = parse_hashtables_blob(&buf).unwrap();
        assert_eq!(read_back, h);
    }

    #[test]
    fn crc_corruption_detected() {
        let h = sample();
        let mut buf = Vec::new();
        write_hashtables(&mut buf, &h).unwrap();
        let mid = buf.len() / 2;
        buf[mid] ^= 0xFF;
        let err = parse_hashtables_blob(&buf).unwrap_err();
        assert!(matches!(err, StorageError::CrcMismatch { .. }));
    }
}
