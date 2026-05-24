//! `sentences.bin` — per-article sentence arena (STORAGE.md §4,
//! decision-resolved option B; sibling of [`crate::paragraphs`]).
//!
//! Each sentence stores its hash, its (often-cleared) value, and the
//! flat list of token ids that make up the sentence in document
//! order. Resume-from-disk replays of a new revision look up
//! candidate sentences by hash (via [`crate::hashtables`]) and read
//! their words from here.
//!
//! Layout:
//!
//! ```text
//! header (16 bytes):
//!   "WWSN"           magic
//!   u16 BE           version
//!   u16 BE           reserved
//!   u32 BE           n_sentences
//!   u32 BE           reserved
//!
//! data section: for each sentence in arena order:
//!   varint u64       hash_value length
//!   raw bytes        hash_value (UTF-8)
//!   varint u64       value length (often 0)
//!   raw bytes        value (UTF-8)
//!   varint u64       n_words
//!   varint i64       token_id delta (zigzag, from prev — first from 0)
//!                    repeated n_words times
//!
//! trailer (8 bytes):
//!   "NSWW"           magic
//!   u32 BE           CRC32 of preceding bytes
//! ```

use std::io::Write;

use crate::codec::{
    crc32, read_u16_be, read_u32_be, read_varint_i64, read_varint_u64, write_u16_be,
    write_u32_be, write_varint_i64, write_varint_u64,
};
use crate::{Result, SCHEMA_VERSION, StorageError};

pub const MAGIC_HEAD: &[u8; 4] = b"WWSN";
pub const MAGIC_TAIL: &[u8; 4] = b"NSWW";
const FILE_NAME: &str = "sentences.bin";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSentence {
    pub hash_value: String,
    pub value: String,
    pub words: Vec<u32>,
}

pub fn write_sentences<W: Write>(w: &mut W, sentences: &[StoredSentence]) -> Result<()> {
    let n = u32::try_from(sentences.len()).map_err(|_| StorageError::Malformed {
        file: FILE_NAME,
        detail: format!("too many sentences ({})", sentences.len()),
    })?;

    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(MAGIC_HEAD);
    write_u16_be(&mut body, SCHEMA_VERSION)?;
    write_u16_be(&mut body, 0)?;
    write_u32_be(&mut body, n)?;
    write_u32_be(&mut body, 0)?;

    for s in sentences {
        write_varint_u64(&mut body, s.hash_value.len() as u64)?;
        body.extend_from_slice(s.hash_value.as_bytes());
        write_varint_u64(&mut body, s.value.len() as u64)?;
        body.extend_from_slice(s.value.as_bytes());
        write_varint_u64(&mut body, s.words.len() as u64)?;
        let mut prev: i64 = 0;
        for &tid in &s.words {
            let delta = tid as i64 - prev;
            write_varint_i64(&mut body, delta)?;
            prev = tid as i64;
        }
    }

    let crc = crc32(&body);
    body.extend_from_slice(MAGIC_TAIL);
    body.extend_from_slice(&crc.to_be_bytes());
    w.write_all(&body)?;
    Ok(())
}

pub fn parse_sentences_blob(all: &[u8]) -> Result<Vec<StoredSentence>> {
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
        out.push(read_one_sentence(&mut cur)?);
    }
    Ok(out)
}

fn read_one_sentence(cur: &mut std::io::Cursor<&[u8]>) -> Result<StoredSentence> {
    let hash_value = read_lp_utf8(cur, "hash_value")?;
    let value = read_lp_utf8(cur, "value")?;
    let n_words = read_varint_u64(cur, FILE_NAME)? as usize;
    let mut words = Vec::with_capacity(n_words);
    let mut prev: i64 = 0;
    for _ in 0..n_words {
        let delta = read_varint_i64(cur, FILE_NAME)?;
        let v = prev + delta;
        if !(0..=u32::MAX as i64).contains(&v) {
            return Err(StorageError::Malformed {
                file: FILE_NAME,
                detail: format!("token_id out of u32 range: {v}"),
            });
        }
        words.push(v as u32);
        prev = v;
    }
    Ok(StoredSentence {
        hash_value,
        value,
        words,
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

    fn sample() -> Vec<StoredSentence> {
        vec![
            StoredSentence {
                hash_value: "s00".into(),
                value: "Hello there friend".into(),
                words: vec![0, 1, 2, 3],
            },
            StoredSentence {
                hash_value: "s01".into(),
                value: "".into(),
                words: vec![4, 5],
            },
            StoredSentence {
                hash_value: "s10".into(),
                value: "".into(),
                // Token ids drawn from the same arena — small values,
                // backward delta from prev (5 → 2).
                words: vec![2, 6, 7],
            },
        ]
    }

    #[test]
    fn round_trip() {
        let ss = sample();
        let mut buf = Vec::new();
        write_sentences(&mut buf, &ss).unwrap();
        let back = parse_sentences_blob(&buf).unwrap();
        assert_eq!(back, ss);
    }

    #[test]
    fn round_trip_empty() {
        let ss: Vec<StoredSentence> = vec![];
        let mut buf = Vec::new();
        write_sentences(&mut buf, &ss).unwrap();
        let back = parse_sentences_blob(&buf).unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn round_trip_sentence_with_no_words() {
        let ss = vec![StoredSentence {
            hash_value: "empty".into(),
            value: "".into(),
            words: vec![],
        }];
        let mut buf = Vec::new();
        write_sentences(&mut buf, &ss).unwrap();
        let back = parse_sentences_blob(&buf).unwrap();
        assert_eq!(back, ss);
    }

    #[test]
    fn crc_corruption_detected() {
        let ss = sample();
        let mut buf = Vec::new();
        write_sentences(&mut buf, &ss).unwrap();
        let mid = buf.len() / 2;
        buf[mid] ^= 0xFF;
        let err = parse_sentences_blob(&buf).unwrap_err();
        assert!(matches!(err, StorageError::CrcMismatch { .. }));
    }
}
