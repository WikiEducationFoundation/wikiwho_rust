//! `strings.bin` — interned token strings (STORAGE.md §2.2).
//!
//! Per-article symbol table. Token strings in WikiWho are lowercased
//! and short; most articles end up with tens of thousands of unique
//! strings even after millions of token instances. Storing them once
//! and referring to them by index keeps `tokens.bin` compact.
//!
//! On-disk layout:
//!
//! ```text
//! header (16 bytes):
//!   "WWST"                              magic
//!   u16 BE                              format version = 1
//!   u16 BE                              reserved = 0
//!   u32 BE                              n_strings
//!   u32 BE                              total bytes of string data
//!
//! index table (8 × n_strings bytes):
//!   for each string i:
//!     u32 BE offset into string data
//!     u32 BE length in bytes
//!
//! string data: UTF-8 bytes, no separators
//!
//! trailer:
//!   "TSWW"                              magic
//!   u32 BE CRC32 of all preceding bytes
//! ```

use std::io::{Read, Seek, SeekFrom, Write};

use crate::codec::{crc32, read_u16_be, read_u32_be, write_u16_be, write_u32_be};
use crate::{Result, SCHEMA_VERSION, StorageError};

pub const MAGIC_HEAD: &[u8; 4] = b"WWST";
pub const MAGIC_TAIL: &[u8; 4] = b"TSWW";
const FILE_NAME: &str = "strings.bin";

/// Write the interned string table to `w`. The order of `strings` is
/// preserved — index `i` in the file resolves to `strings[i]`.
pub fn write_strings<W: Write>(w: &mut W, strings: &[&str]) -> Result<()> {
    let n_strings = u32::try_from(strings.len()).map_err(|_| StorageError::Malformed {
        file: "strings.bin",
        detail: format!("too many strings ({})", strings.len()),
    })?;
    let total_bytes: u64 = strings.iter().map(|s| s.len() as u64).sum();
    let total_bytes = u32::try_from(total_bytes).map_err(|_| StorageError::Malformed {
        file: "strings.bin",
        detail: format!("total string bytes {total_bytes} > u32"),
    })?;

    let mut body: Vec<u8> = Vec::with_capacity(16 + 8 * strings.len() + total_bytes as usize);
    body.extend_from_slice(MAGIC_HEAD);
    write_u16_be(&mut body, SCHEMA_VERSION)?;
    write_u16_be(&mut body, 0)?;
    write_u32_be(&mut body, n_strings)?;
    write_u32_be(&mut body, total_bytes)?;

    let mut running_offset: u32 = 0;
    for s in strings {
        write_u32_be(&mut body, running_offset)?;
        let len = u32::try_from(s.len()).map_err(|_| StorageError::Malformed {
            file: "strings.bin",
            detail: "string longer than 4GB".to_string(),
        })?;
        write_u32_be(&mut body, len)?;
        running_offset = running_offset.checked_add(len).ok_or_else(|| {
            StorageError::Malformed {
                file: "strings.bin",
                detail: "string offset overflowed u32".to_string(),
            }
        })?;
    }
    for s in strings {
        body.extend_from_slice(s.as_bytes());
    }

    let crc = crc32(&body);
    body.extend_from_slice(MAGIC_TAIL);
    body.extend_from_slice(&crc.to_be_bytes());

    w.write_all(&body)?;
    Ok(())
}

/// Read every interned string from `r` in index order. Validates magic,
/// version, and CRC.
pub fn read_strings<R: Read + Seek>(r: &mut R) -> Result<Vec<String>> {
    // Slurp the file. The table is small enough (typical: <10 MB
    // uncompressed) that one allocation is cheaper than incremental
    // I/O with multiple seeks.
    r.seek(SeekFrom::Start(0))?;
    let mut all = Vec::new();
    r.read_to_end(&mut all)?;
    parse_strings_blob(&all)
}

/// Parse a fully-buffered `strings.bin` payload.
pub fn parse_strings_blob(all: &[u8]) -> Result<Vec<String>> {
    if all.len() < 16 + 8 {
        return Err(StorageError::UnexpectedEof { file: FILE_NAME });
    }

    // CRC covers everything before the 8-byte trailer.
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
    // payload[6..8] reserved
    let n_strings = read_u32_be(&payload[8..12]) as usize;
    let total_bytes = read_u32_be(&payload[12..16]) as usize;

    let index_start = 16;
    let index_end = index_start + 8 * n_strings;
    let data_start = index_end;
    let data_end = data_start + total_bytes;
    if data_end != payload.len() {
        return Err(StorageError::Malformed {
            file: FILE_NAME,
            detail: format!(
                "header says {} strings + {} data bytes, but payload is {} bytes (expected {})",
                n_strings,
                total_bytes,
                payload.len(),
                data_end
            ),
        });
    }

    let mut out = Vec::with_capacity(n_strings);
    for i in 0..n_strings {
        let entry_start = index_start + 8 * i;
        let offset = read_u32_be(&payload[entry_start..entry_start + 4]) as usize;
        let length = read_u32_be(&payload[entry_start + 4..entry_start + 8]) as usize;
        let s_start = data_start + offset;
        let s_end = s_start + length;
        if s_end > data_end {
            return Err(StorageError::Malformed {
                file: FILE_NAME,
                detail: format!(
                    "string {i} extends past data segment ({s_end} > {data_end})"
                ),
            });
        }
        let s = std::str::from_utf8(&payload[s_start..s_end]).map_err(|e| {
            StorageError::Malformed {
                file: FILE_NAME,
                detail: format!("string {i} is not UTF-8: {e}"),
            }
        })?;
        out.push(s.to_string());
    }

    Ok(out)
}

/// Random-access lookup table over a mmap'd strings.bin payload. Used
/// when we want to resolve one or a few string ids without walking
/// the whole table.
///
/// Construction validates magic and CRC; lookups are constant-time.
pub struct StringsIndex<'a> {
    payload: &'a [u8],
    index_start: usize,
    data_start: usize,
    n_strings: usize,
    data_len: usize,
}

impl<'a> StringsIndex<'a> {
    /// Validate the supplied payload (full file contents) and return a
    /// random-access handle.
    pub fn new(all: &'a [u8]) -> Result<Self> {
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
        let n_strings = read_u32_be(&payload[8..12]) as usize;
        let total_bytes = read_u32_be(&payload[12..16]) as usize;
        let index_start = 16;
        let data_start = index_start + 8 * n_strings;
        Ok(Self {
            payload,
            index_start,
            data_start,
            n_strings,
            data_len: total_bytes,
        })
    }

    pub fn len(&self) -> usize {
        self.n_strings
    }

    pub fn is_empty(&self) -> bool {
        self.n_strings == 0
    }

    /// Resolve a string id to a borrowed `&str`. Returns `None` if `id`
    /// is out of range.
    pub fn get(&self, id: u32) -> Option<&'a str> {
        let i = id as usize;
        if i >= self.n_strings {
            return None;
        }
        let entry_start = self.index_start + 8 * i;
        let offset = read_u32_be(&self.payload[entry_start..entry_start + 4]) as usize;
        let length = read_u32_be(&self.payload[entry_start + 4..entry_start + 8]) as usize;
        let s_start = self.data_start + offset;
        let s_end = s_start + length;
        if s_end > self.data_start + self.data_len {
            return None;
        }
        std::str::from_utf8(&self.payload[s_start..s_end]).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn round_trip_simple() {
        let inputs = vec!["the", "quick", "brown", "fox"];
        let mut buf = Vec::new();
        write_strings(&mut buf, &inputs).unwrap();
        let mut cur = Cursor::new(buf);
        let read_back = read_strings(&mut cur).unwrap();
        assert_eq!(read_back, inputs);
    }

    #[test]
    fn round_trip_empty() {
        let inputs: Vec<&str> = vec![];
        let mut buf = Vec::new();
        write_strings(&mut buf, &inputs).unwrap();
        let mut cur = Cursor::new(buf);
        let read_back = read_strings(&mut cur).unwrap();
        assert!(read_back.is_empty());
    }

    #[test]
    fn round_trip_unicode() {
        let inputs = vec!["café", "naïve", "日本語", "🦀", ""];
        let mut buf = Vec::new();
        write_strings(&mut buf, &inputs).unwrap();
        let mut cur = Cursor::new(buf);
        let read_back = read_strings(&mut cur).unwrap();
        assert_eq!(read_back, inputs);
    }

    #[test]
    fn random_access_lookups() {
        let inputs = vec!["alpha", "beta", "gamma", "delta"];
        let mut buf = Vec::new();
        write_strings(&mut buf, &inputs).unwrap();
        let idx = StringsIndex::new(&buf).unwrap();
        assert_eq!(idx.len(), 4);
        assert_eq!(idx.get(0), Some("alpha"));
        assert_eq!(idx.get(1), Some("beta"));
        assert_eq!(idx.get(2), Some("gamma"));
        assert_eq!(idx.get(3), Some("delta"));
        assert_eq!(idx.get(4), None);
    }

    #[test]
    fn crc_corruption_is_detected() {
        let inputs = vec!["hello"];
        let mut buf = Vec::new();
        write_strings(&mut buf, &inputs).unwrap();
        // Flip a byte inside the string data.
        let body_pos = buf.len() - 8 - 5; // last 8 bytes are trailer, 5 = "hello".len()
        buf[body_pos] ^= 0x40;
        let mut cur = Cursor::new(buf);
        let err = read_strings(&mut cur).unwrap_err();
        assert!(matches!(err, StorageError::CrcMismatch { .. }));
    }

    #[test]
    fn bad_head_magic_is_detected() {
        let inputs = vec!["x"];
        let mut buf = Vec::new();
        write_strings(&mut buf, &inputs).unwrap();
        buf[0] = b'!';
        let mut cur = Cursor::new(buf);
        let err = read_strings(&mut cur).unwrap_err();
        // CRC catches the corrupted head before magic check kicks in -
        // either error is fine to surface, but we should NOT silently
        // round-trip a corrupted file.
        assert!(matches!(
            err,
            StorageError::BadMagic { .. } | StorageError::CrcMismatch { .. }
        ));
    }

    #[test]
    fn version_too_new_is_rejected() {
        let inputs = vec!["hi"];
        let mut buf = Vec::new();
        write_strings(&mut buf, &inputs).unwrap();
        // Overwrite version field (bytes 4-5) with a future version.
        buf[4] = 0;
        buf[5] = 0xFF;
        // Recompute CRC so we exercise the version check, not the CRC.
        let payload_len = buf.len() - 8;
        let new_crc = crc32(&buf[..payload_len]);
        buf[payload_len + 4..payload_len + 8].copy_from_slice(&new_crc.to_be_bytes());
        let mut cur = Cursor::new(buf);
        let err = read_strings(&mut cur).unwrap_err();
        assert!(matches!(err, StorageError::UnsupportedVersion { .. }));
    }
}
