//! `tokens.bin` — per-lifetime-token records (STORAGE.md §2.3).
//!
//! One record per `token_id` in id order. A record carries:
//!
//! - `string_id` — index into `strings.bin`
//! - `origin_rev_id` — first revision that introduced the token
//! - `last_rev_id` — most recent revision the token appeared in
//! - `inbound` — revisions where the token was re-added after a delete
//! - `outbound` — revisions where the token was deleted
//!
//! All numeric fields are varint-encoded; rev-id chains are
//! delta-encoded to exploit the fact that adjacent tokens in id order
//! were usually introduced in the same (or nearby) revision, and
//! inbound/outbound lists for a single token are typically short and
//! chronologically ordered.
//!
//! Layout:
//!
//! ```text
//! header (16 bytes):
//!   "WWTK"           magic
//!   u16 BE           version = 1
//!   u16 BE           reserved
//!   u32 BE           n_tokens
//!   u32 BE           reserved
//!
//! records: for each token in id order:
//!   varint zigzag:   string_id delta from previous (absolute for token 0)
//!   varint zigzag:   origin_rev_id delta from previous (absolute for token 0)
//!   varint zigzag:   last_rev_id - origin_rev_id
//!   varint u64:      n_inbound
//!   varint zigzag × n_inbound:
//!                    inbound[0] delta from origin_rev_id,
//!                    inbound[i] delta from inbound[i-1]
//!   varint u64:      n_outbound
//!   varint zigzag × n_outbound:
//!                    outbound[0] delta from origin_rev_id,
//!                    outbound[i] delta from outbound[i-1]
//!
//! trailer:
//!   "KTWW"           magic
//!   u32 BE           CRC32 of preceding bytes
//! ```
//!
//! All rev-id chains use **signed (zigzag) deltas**, even where the
//! chronological order would suggest monotonicity. Wikipedia's
//! pre-2002 rev_ids violate the monotonic-in-time-equals-monotonic-
//! in-rev-id assumption: enwiki has revs with small (~38k) rev_ids
//! that were timestamped *after* revs with much larger (~275k) ones,
//! a quirk of the 2002 database migration. The algorithm processes
//! revs in timestamp order, so these chains can move backward in
//! rev_id space. Photosynthesis (en/24544) trips this — see
//! `notes/2026-05-23-storage-scaffold.md`.

use std::io::Write;

use crate::codec::{
    crc32, read_u16_be, read_u32_be, read_varint_i64, read_varint_u64, write_u16_be, write_u32_be,
    write_varint_i64, write_varint_u64,
};
use crate::{Result, SCHEMA_VERSION, StorageError};

/// Cap on rev_id values we encode as signed deltas. Wikipedia rev_ids
/// today are in the low 10⁹; `i64::MAX` is comfortable.
fn i64_from_revid(r: u64) -> Result<i64> {
    if r > i64::MAX as u64 {
        return Err(StorageError::Malformed {
            file: FILE_NAME,
            detail: format!("rev_id {r} exceeds i64::MAX (signed-delta encoding limit)"),
        });
    }
    Ok(r as i64)
}

fn revid_from_i64(v: i64) -> Result<u64> {
    if v < 0 {
        return Err(StorageError::Malformed {
            file: FILE_NAME,
            detail: format!("decoded rev_id is negative: {v}"),
        });
    }
    Ok(v as u64)
}

pub const MAGIC_HEAD: &[u8; 4] = b"WWTK";
pub const MAGIC_TAIL: &[u8; 4] = b"KTWW";
const FILE_NAME: &str = "tokens.bin";

/// One token record's on-disk projection. The persistence layer takes
/// these in id order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredToken {
    pub string_id: u32,
    pub origin_rev_id: u64,
    pub last_rev_id: u64,
    pub inbound: Vec<u64>,
    pub outbound: Vec<u64>,
}

/// Write a sequence of tokens to `w`. The order is preserved — index
/// `i` in the file is `token_id i`.
pub fn write_tokens<W: Write>(w: &mut W, tokens: &[StoredToken]) -> Result<()> {
    let n_tokens = u32::try_from(tokens.len()).map_err(|_| StorageError::Malformed {
        file: FILE_NAME,
        detail: format!("too many tokens ({})", tokens.len()),
    })?;

    let mut body: Vec<u8> = Vec::with_capacity(16 + 8 * tokens.len());
    body.extend_from_slice(MAGIC_HEAD);
    write_u16_be(&mut body, SCHEMA_VERSION)?;
    write_u16_be(&mut body, 0)?;
    write_u32_be(&mut body, n_tokens)?;
    write_u32_be(&mut body, 0)?;

    let mut prev_string_id: i64 = 0;
    let mut prev_origin: i64 = 0;
    for tok in tokens {
        let string_delta = tok.string_id as i64 - prev_string_id;
        write_varint_i64(&mut body, string_delta)?;
        prev_string_id = tok.string_id as i64;

        // Signed delta. See module doc for why monotonicity is not
        // guaranteed (pre-2002 enwiki rev_id quirk).
        let origin_signed = i64_from_revid(tok.origin_rev_id)?;
        write_varint_i64(&mut body, origin_signed - prev_origin)?;
        prev_origin = origin_signed;

        let last_signed = i64_from_revid(tok.last_rev_id)?;
        write_varint_i64(&mut body, last_signed - origin_signed)?;

        write_varint_u64(&mut body, tok.inbound.len() as u64)?;
        let mut prev = origin_signed;
        for &r in &tok.inbound {
            let r_signed = i64_from_revid(r)?;
            write_varint_i64(&mut body, r_signed - prev)?;
            prev = r_signed;
        }

        write_varint_u64(&mut body, tok.outbound.len() as u64)?;
        let mut prev = origin_signed;
        for &r in &tok.outbound {
            let r_signed = i64_from_revid(r)?;
            write_varint_i64(&mut body, r_signed - prev)?;
            prev = r_signed;
        }
    }

    let crc = crc32(&body);
    body.extend_from_slice(MAGIC_TAIL);
    body.extend_from_slice(&crc.to_be_bytes());

    w.write_all(&body)?;
    Ok(())
}

/// Read tokens from a fully-buffered `tokens.bin` payload.
pub fn parse_tokens_blob(all: &[u8]) -> Result<Vec<StoredToken>> {
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
    let n_tokens = read_u32_be(&payload[8..12]) as usize;
    // payload[12..16] reserved

    let mut cur = std::io::Cursor::new(&payload[16..]);
    let mut out = Vec::with_capacity(n_tokens);
    let mut prev_string_id: i64 = 0;
    let mut prev_origin: i64 = 0;

    for _ in 0..n_tokens {
        let string_delta = read_varint_i64(&mut cur, FILE_NAME)?;
        let string_id_signed = prev_string_id + string_delta;
        if string_id_signed < 0 || string_id_signed > u32::MAX as i64 {
            return Err(StorageError::Malformed {
                file: FILE_NAME,
                detail: format!("string_id out of u32 range: {string_id_signed}"),
            });
        }
        let string_id = string_id_signed as u32;
        prev_string_id = string_id_signed;

        let origin_delta = read_varint_i64(&mut cur, FILE_NAME)?;
        let origin_signed = prev_origin.checked_add(origin_delta).ok_or_else(|| {
            StorageError::Malformed {
                file: FILE_NAME,
                detail: "origin_rev_id delta overflowed i64".to_string(),
            }
        })?;
        let origin_rev_id = revid_from_i64(origin_signed)?;
        prev_origin = origin_signed;

        let last_delta = read_varint_i64(&mut cur, FILE_NAME)?;
        let last_signed = origin_signed.checked_add(last_delta).ok_or_else(|| {
            StorageError::Malformed {
                file: FILE_NAME,
                detail: "last_rev_id delta overflowed i64".to_string(),
            }
        })?;
        let last_rev_id = revid_from_i64(last_signed)?;

        let n_inbound = read_varint_u64(&mut cur, FILE_NAME)? as usize;
        let mut inbound = Vec::with_capacity(n_inbound);
        let mut prev = origin_signed;
        for _ in 0..n_inbound {
            let d = read_varint_i64(&mut cur, FILE_NAME)?;
            let v = prev.checked_add(d).ok_or_else(|| StorageError::Malformed {
                file: FILE_NAME,
                detail: "inbound rev_id delta overflowed i64".to_string(),
            })?;
            inbound.push(revid_from_i64(v)?);
            prev = v;
        }

        let n_outbound = read_varint_u64(&mut cur, FILE_NAME)? as usize;
        let mut outbound = Vec::with_capacity(n_outbound);
        let mut prev = origin_signed;
        for _ in 0..n_outbound {
            let d = read_varint_i64(&mut cur, FILE_NAME)?;
            let v = prev.checked_add(d).ok_or_else(|| StorageError::Malformed {
                file: FILE_NAME,
                detail: "outbound rev_id delta overflowed i64".to_string(),
            })?;
            outbound.push(revid_from_i64(v)?);
            prev = v;
        }

        out.push(StoredToken {
            string_id,
            origin_rev_id,
            last_rev_id,
            inbound,
            outbound,
        });
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tokens() -> Vec<StoredToken> {
        vec![
            StoredToken {
                string_id: 0,
                origin_rev_id: 100,
                last_rev_id: 100,
                inbound: vec![],
                outbound: vec![],
            },
            StoredToken {
                string_id: 1,
                origin_rev_id: 100,
                last_rev_id: 200,
                inbound: vec![150, 175, 200],
                outbound: vec![140, 160],
            },
            StoredToken {
                string_id: 0, // same string as token 0 (negative delta)
                origin_rev_id: 200,
                last_rev_id: 200,
                inbound: vec![],
                outbound: vec![],
            },
        ]
    }

    #[test]
    fn round_trip_simple() {
        let toks = sample_tokens();
        let mut buf = Vec::new();
        write_tokens(&mut buf, &toks).unwrap();
        let read_back = parse_tokens_blob(&buf).unwrap();
        assert_eq!(read_back, toks);
    }

    #[test]
    fn round_trip_empty() {
        let toks: Vec<StoredToken> = vec![];
        let mut buf = Vec::new();
        write_tokens(&mut buf, &toks).unwrap();
        let read_back = parse_tokens_blob(&buf).unwrap();
        assert!(read_back.is_empty());
    }

    #[test]
    fn round_trip_many_revisions_same_origin() {
        // Tokens introduced together in one revision — origin_delta = 0
        // for each but the first.
        let toks: Vec<StoredToken> = (0..100)
            .map(|i| StoredToken {
                string_id: i as u32,
                origin_rev_id: 500,
                last_rev_id: 500,
                inbound: vec![],
                outbound: vec![],
            })
            .collect();
        let mut buf = Vec::new();
        write_tokens(&mut buf, &toks).unwrap();
        let read_back = parse_tokens_blob(&buf).unwrap();
        assert_eq!(read_back, toks);

        // Sanity: the body shouldn't be more than a few bytes per token
        // (each token: 1 byte string_delta + 1 byte origin_delta + 1 byte
        // last_delta + 1 byte n_in + 1 byte n_out ≈ 5 bytes / token).
        assert!(buf.len() < 16 + 8 + 100 * 8, "actual {}", buf.len());
    }

    #[test]
    fn round_trip_long_inbound_outbound() {
        // Lots of inbound/outbound deltas to exercise the chain.
        let toks = vec![StoredToken {
            string_id: 0,
            origin_rev_id: 1000,
            last_rev_id: 2000,
            inbound: (1100..1200).step_by(2).collect(),
            outbound: (1050..1200).step_by(5).collect(),
        }];
        let mut buf = Vec::new();
        write_tokens(&mut buf, &toks).unwrap();
        let read_back = parse_tokens_blob(&buf).unwrap();
        assert_eq!(read_back, toks);
    }

    #[test]
    fn non_monotonic_origin_round_trips() {
        // Pre-2002 enwiki revs have small rev_ids that come *after*
        // bigger ones in time. Token at origin 275452 introduced first,
        // then a token at origin 38939 introduced later. Both must
        // round-trip cleanly.
        let toks = vec![
            StoredToken {
                string_id: 0,
                origin_rev_id: 275452,
                last_rev_id: 275452,
                inbound: vec![],
                outbound: vec![],
            },
            StoredToken {
                string_id: 1,
                origin_rev_id: 38939,
                last_rev_id: 38939,
                inbound: vec![],
                outbound: vec![],
            },
        ];
        let mut buf = Vec::new();
        write_tokens(&mut buf, &toks).unwrap();
        let read_back = parse_tokens_blob(&buf).unwrap();
        assert_eq!(read_back, toks);
    }

    #[test]
    fn last_before_origin_round_trips() {
        // A token introduced in 2001 (rev 275452) and then matched in
        // a 2002 revision with smaller rev_id (38939). The algorithm
        // would set last_rev_id to the chronologically-later but
        // numerically-smaller rev.
        let toks = vec![StoredToken {
            string_id: 0,
            origin_rev_id: 275452,
            last_rev_id: 38939,
            inbound: vec![38939],
            outbound: vec![],
        }];
        let mut buf = Vec::new();
        write_tokens(&mut buf, &toks).unwrap();
        let read_back = parse_tokens_blob(&buf).unwrap();
        assert_eq!(read_back, toks);
    }

    #[test]
    fn crc_corruption_detected() {
        let toks = sample_tokens();
        let mut buf = Vec::new();
        write_tokens(&mut buf, &toks).unwrap();
        let mid = buf.len() / 2;
        buf[mid] ^= 0xFF;
        let err = parse_tokens_blob(&buf).unwrap_err();
        assert!(matches!(err, StorageError::CrcMismatch { .. }));
    }
}
