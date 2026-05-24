//! `paragraphs.bin` — per-article paragraph arena (STORAGE.md §4,
//! decision-resolved option B).
//!
//! Persisting this file is what unblocks the algorithm's
//! resume-from-disk path: once paragraphs are on disk, a newly
//! arrived revision can match against them without re-replaying the
//! article's full history. Mirrors the [`tokens.bin`] arena pattern —
//! arena id == record index, written in arena order.
//!
//! Each paragraph carries its `ordered_sentences` (a flat list of
//! `(sentence_hash, sentence_id)` pairs in document order). The
//! `sentences: HashMap<Hash, Vec<SentenceId>>` map on the in-memory
//! [`Paragraph`] is derivable from this flat list by grouping, so we
//! don't store it separately. Per-paragraph `value` is usually empty
//! (the reference clears it after first insertion, see
//! `wikiwho.py:314`), but we persist whatever's there to round-trip
//! truly.
//!
//! Layout:
//!
//! ```text
//! header (16 bytes):
//!   "WWPG"           magic
//!   u16 BE           version
//!   u16 BE           reserved
//!   u32 BE           n_paragraphs
//!   u32 BE           reserved
//!
//! data section: for each paragraph in arena order:
//!   varint u64       hash_value length
//!   raw bytes        hash_value (UTF-8)
//!   varint u64       value length (often 0)
//!   raw bytes        value (UTF-8)
//!   varint u64       n_ordered_sentences
//!   for each in document order:
//!     varint u64     sentence_hash length
//!     raw bytes      sentence_hash (UTF-8)
//!     varint i64     sentence_id delta (zigzag, from prev — first from 0)
//!
//! trailer (8 bytes):
//!   "GPWW"           magic
//!   u32 BE           CRC32 of preceding bytes
//! ```
//!
//! Sentence-id deltas are signed because the arena order need not be
//! locally ascending — a paragraph that re-references an earlier
//! sentence is normal and frequent. Same rationale as the rev-id
//! deltas in `tokens.bin`.

use std::io::Write;

use crate::codec::{
    crc32, read_u16_be, read_u32_be, read_varint_i64, read_varint_u64, write_u16_be,
    write_u32_be, write_varint_i64, write_varint_u64,
};
use crate::{Result, SCHEMA_VERSION, StorageError};

pub const MAGIC_HEAD: &[u8; 4] = b"WWPG";
pub const MAGIC_TAIL: &[u8; 4] = b"GPWW";
const FILE_NAME: &str = "paragraphs.bin";

/// One entry of a paragraph's ordered sentence list. The two fields
/// are persisted together so the grouped `sentences` map on the
/// in-memory [`Paragraph`] can be reconstructed by walking the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredOrderedSentence {
    pub hash: String,
    pub sentence_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredParagraph {
    pub hash_value: String,
    pub value: String,
    pub ordered_sentences: Vec<StoredOrderedSentence>,
}

pub fn write_paragraphs<W: Write>(w: &mut W, paragraphs: &[StoredParagraph]) -> Result<()> {
    let n = u32::try_from(paragraphs.len()).map_err(|_| StorageError::Malformed {
        file: FILE_NAME,
        detail: format!("too many paragraphs ({})", paragraphs.len()),
    })?;

    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(MAGIC_HEAD);
    write_u16_be(&mut body, SCHEMA_VERSION)?;
    write_u16_be(&mut body, 0)?;
    write_u32_be(&mut body, n)?;
    write_u32_be(&mut body, 0)?;

    for p in paragraphs {
        write_varint_u64(&mut body, p.hash_value.len() as u64)?;
        body.extend_from_slice(p.hash_value.as_bytes());
        write_varint_u64(&mut body, p.value.len() as u64)?;
        body.extend_from_slice(p.value.as_bytes());
        write_varint_u64(&mut body, p.ordered_sentences.len() as u64)?;
        let mut prev: i64 = 0;
        for s in &p.ordered_sentences {
            write_varint_u64(&mut body, s.hash.len() as u64)?;
            body.extend_from_slice(s.hash.as_bytes());
            let delta = s.sentence_id as i64 - prev;
            write_varint_i64(&mut body, delta)?;
            prev = s.sentence_id as i64;
        }
    }

    let crc = crc32(&body);
    body.extend_from_slice(MAGIC_TAIL);
    body.extend_from_slice(&crc.to_be_bytes());
    w.write_all(&body)?;
    Ok(())
}

pub fn parse_paragraphs_blob(all: &[u8]) -> Result<Vec<StoredParagraph>> {
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
    let n = read_u32_be(&payload[8..12]) as usize;

    let mut cur = std::io::Cursor::new(&payload[16..]);
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(read_one_paragraph(&mut cur)?);
    }
    Ok(out)
}

fn read_one_paragraph(cur: &mut std::io::Cursor<&[u8]>) -> Result<StoredParagraph> {
    let hash_value = read_lp_utf8(cur, "hash_value")?;
    let value = read_lp_utf8(cur, "value")?;
    let n_sentences = read_varint_u64(cur, FILE_NAME)? as usize;
    let mut ordered_sentences = Vec::with_capacity(n_sentences);
    let mut prev: i64 = 0;
    for _ in 0..n_sentences {
        let hash = read_lp_utf8(cur, "sentence_hash")?;
        let delta = read_varint_i64(cur, FILE_NAME)?;
        let v = prev + delta;
        if !(0..=u32::MAX as i64).contains(&v) {
            return Err(StorageError::Malformed {
                file: FILE_NAME,
                detail: format!("sentence_id out of u32 range: {v}"),
            });
        }
        ordered_sentences.push(StoredOrderedSentence {
            hash,
            sentence_id: v as u32,
        });
        prev = v;
    }
    Ok(StoredParagraph {
        hash_value,
        value,
        ordered_sentences,
    })
}

fn read_lp_utf8(cur: &mut std::io::Cursor<&[u8]>, field: &'static str) -> Result<String> {
    let n = read_varint_u64(cur, FILE_NAME)? as usize;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<StoredParagraph> {
        vec![
            StoredParagraph {
                hash_value: "hp0".into(),
                value: "".into(),
                ordered_sentences: vec![
                    StoredOrderedSentence { hash: "s00".into(), sentence_id: 0 },
                    StoredOrderedSentence { hash: "s01".into(), sentence_id: 1 },
                ],
            },
            StoredParagraph {
                hash_value: "hp1".into(),
                value: "First paragraph text body.".into(),
                ordered_sentences: vec![
                    StoredOrderedSentence { hash: "s10".into(), sentence_id: 2 },
                ],
            },
            // Paragraph that re-references earlier sentences (the
                // delta encoder must handle backward jumps cleanly).
            StoredParagraph {
                hash_value: "hp2".into(),
                value: "".into(),
                ordered_sentences: vec![
                    StoredOrderedSentence { hash: "s00".into(), sentence_id: 0 },
                    StoredOrderedSentence { hash: "s10".into(), sentence_id: 2 },
                ],
            },
        ]
    }

    #[test]
    fn round_trip() {
        let ps = sample();
        let mut buf = Vec::new();
        write_paragraphs(&mut buf, &ps).unwrap();
        let back = parse_paragraphs_blob(&buf).unwrap();
        assert_eq!(back, ps);
    }

    #[test]
    fn round_trip_empty() {
        let ps: Vec<StoredParagraph> = vec![];
        let mut buf = Vec::new();
        write_paragraphs(&mut buf, &ps).unwrap();
        let back = parse_paragraphs_blob(&buf).unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn round_trip_paragraph_with_no_sentences() {
        let ps = vec![StoredParagraph {
            hash_value: "lonely".into(),
            value: "no sentences here".into(),
            ordered_sentences: vec![],
        }];
        let mut buf = Vec::new();
        write_paragraphs(&mut buf, &ps).unwrap();
        let back = parse_paragraphs_blob(&buf).unwrap();
        assert_eq!(back, ps);
    }

    #[test]
    fn crc_corruption_detected() {
        let ps = sample();
        let mut buf = Vec::new();
        write_paragraphs(&mut buf, &ps).unwrap();
        let mid = buf.len() / 2;
        buf[mid] ^= 0xFF;
        let err = parse_paragraphs_blob(&buf).unwrap_err();
        assert!(matches!(err, StorageError::CrcMismatch { .. }));
    }

    #[test]
    fn bad_magic_detected() {
        let ps = sample();
        let mut buf = Vec::new();
        write_paragraphs(&mut buf, &ps).unwrap();
        buf[0] = b'X';
        let err = parse_paragraphs_blob(&buf).unwrap_err();
        // CRC fails first because head magic is inside the CRC region.
        assert!(matches!(
            err,
            StorageError::CrcMismatch { .. } | StorageError::BadMagic { .. }
        ));
    }

    #[test]
    fn non_monotonic_sentence_ids_handled() {
        // Sentence ids that go up, then down, then back up.
        let ps = vec![StoredParagraph {
            hash_value: "zigzag".into(),
            value: "".into(),
            ordered_sentences: vec![
                StoredOrderedSentence { hash: "a".into(), sentence_id: 100 },
                StoredOrderedSentence { hash: "b".into(), sentence_id: 5 },
                StoredOrderedSentence { hash: "c".into(), sentence_id: 200 },
                StoredOrderedSentence { hash: "d".into(), sentence_id: 0 },
            ],
        }];
        let mut buf = Vec::new();
        write_paragraphs(&mut buf, &ps).unwrap();
        let back = parse_paragraphs_blob(&buf).unwrap();
        assert_eq!(back, ps);
    }
}
