//! `hashtables.bin` — cross-revision paragraph + sentence hash tables
//! (STORAGE.md §4, Strategy B).
//!
//! **Scope at this commit:** the file persists only the hash-set
//! membership (hash strings + occurrence counts). The full design
//! includes `(rev_id, position)` back-references pointing into
//! `revisions.bin` so the algorithm can resume from disk and apply a
//! new revision. That requires persisting paragraph/sentence arenas,
//! which the current `revisions.bin` format does **not** do (see
//! `crate::revisions` for the deviation rationale). The read path
//! doesn't need either — the wire format is served from
//! `tokens.bin` + `revisions.bin` alone. Extending to the full design
//! is a queued decision; see `notes/decisions-needed.md`.
//!
//! For now the file is a write-time diagnostic: writing produces a
//! parseable blob; reading back yields the same hash set; and an
//! eventual extension can append back-references without breaking
//! the existing header (the version field carries forward).
//!
//! Layout:
//!
//! ```text
//! header (16 bytes):
//!   "WWHT"           magic
//!   u16 BE           version = 1
//!   u16 BE           reserved
//!   u32 BE           n_paragraph_hashes
//!   u32 BE           n_sentence_hashes
//!
//! paragraph entries: for each (in iteration order):
//!   varint u64:      hash length (always 32 for hex-MD5; varint for forward-compat)
//!   raw bytes:       hash (UTF-8)
//!   varint u64:      occurrences count
//!
//! sentence entries: same shape
//!
//! trailer (8 bytes):
//!   "THWW"           magic
//!   u32 BE           CRC32 of preceding bytes
//! ```

use std::io::Write;

use crate::codec::{
    crc32, read_u16_be, read_u32_be, read_varint_u64, write_u16_be, write_u32_be, write_varint_u64,
};
use crate::{Result, SCHEMA_VERSION, StorageError};

pub const MAGIC_HEAD: &[u8; 4] = b"WWHT";
pub const MAGIC_TAIL: &[u8; 4] = b"THWW";
const FILE_NAME: &str = "hashtables.bin";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HashTables {
    pub paragraph_hashes: Vec<(String, u64)>,
    pub sentence_hashes: Vec<(String, u64)>,
}

pub fn write_hashtables<W: Write>(w: &mut W, tables: &HashTables) -> Result<()> {
    let n_p = u32::try_from(tables.paragraph_hashes.len()).map_err(|_| {
        StorageError::Malformed {
            file: FILE_NAME,
            detail: "too many paragraph hashes".to_string(),
        }
    })?;
    let n_s = u32::try_from(tables.sentence_hashes.len()).map_err(|_| {
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

    for (h, n) in &tables.paragraph_hashes {
        write_varint_u64(&mut body, h.len() as u64)?;
        body.extend_from_slice(h.as_bytes());
        write_varint_u64(&mut body, *n)?;
    }
    for (h, n) in &tables.sentence_hashes {
        write_varint_u64(&mut body, h.len() as u64)?;
        body.extend_from_slice(h.as_bytes());
        write_varint_u64(&mut body, *n)?;
    }

    let crc = crc32(&body);
    body.extend_from_slice(MAGIC_TAIL);
    body.extend_from_slice(&crc.to_be_bytes());

    w.write_all(&body)?;
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
    let mut paragraph_hashes = Vec::with_capacity(n_p);
    for _ in 0..n_p {
        paragraph_hashes.push(read_entry(&mut cur)?);
    }
    let mut sentence_hashes = Vec::with_capacity(n_s);
    for _ in 0..n_s {
        sentence_hashes.push(read_entry(&mut cur)?);
    }

    Ok(HashTables {
        paragraph_hashes,
        sentence_hashes,
    })
}

fn read_entry(cur: &mut std::io::Cursor<&[u8]>) -> Result<(String, u64)> {
    let len = read_varint_u64(cur, FILE_NAME)? as usize;
    let pos = cur.position() as usize;
    let inner = *cur.get_ref();
    if pos + len > inner.len() {
        return Err(StorageError::UnexpectedEof { file: FILE_NAME });
    }
    let s = std::str::from_utf8(&inner[pos..pos + len]).map_err(|e| StorageError::Malformed {
        file: FILE_NAME,
        detail: format!("invalid utf8 hash: {e}"),
    })?;
    cur.set_position((pos + len) as u64);
    let count = read_varint_u64(cur, FILE_NAME)?;
    Ok((s.to_string(), count))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> HashTables {
        HashTables {
            paragraph_hashes: vec![
                ("abc123".repeat(5), 1),
                ("deadbeef".repeat(4), 3),
            ],
            sentence_hashes: vec![
                ("ffeeddcc".repeat(4), 1),
                ("11223344".repeat(4), 7),
                ("55667788".repeat(4), 1),
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
