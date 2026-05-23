//! Low-level encoding primitives shared by every binary file.
//!
//! - **Big-endian fixed-width** integers per STORAGE.md §2.x file headers.
//! - **Zigzag-encoded LEB128 varints** (protobuf `sint64` semantics)
//!   for the per-record fields. Zigzag means small *signed* deltas
//!   fit in 1-2 bytes; we use this for delta-encoded rev-id chains
//!   where adjacent values are usually close.
//! - **CRC32 (IEEE)** trailers for each file, computed over
//!   everything except the trailer itself.

use std::io::{Read, Write};

use crate::{Result, StorageError};

/// Stream-style varint writer/reader. All multibyte numeric fields in
/// the binary files (except big-endian fixed-width headers) use these.
pub fn write_varint_u64<W: Write>(w: &mut W, value: u64) -> Result<()> {
    let mut v = value;
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            w.write_all(&[byte])?;
            return Ok(());
        }
        w.write_all(&[byte | 0x80])?;
    }
}

/// Read an unsigned varint from `r`. Returns `Err(UnexpectedEof)` if the
/// stream ends mid-varint.
pub fn read_varint_u64<R: Read>(r: &mut R, file: &'static str) -> Result<u64> {
    let mut value: u64 = 0;
    let mut shift = 0;
    loop {
        let mut buf = [0u8; 1];
        let n = r.read(&mut buf)?;
        if n == 0 {
            return Err(StorageError::UnexpectedEof { file });
        }
        let b = buf[0];
        value |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift >= 64 {
            return Err(StorageError::Malformed {
                file,
                detail: "varint too wide for u64".into(),
            });
        }
    }
}

/// Zigzag-encode an i64 into a u64 (protobuf sint64).
#[inline]
pub fn zigzag_encode(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

/// Zigzag-decode a u64 back into an i64.
#[inline]
pub fn zigzag_decode(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

pub fn write_varint_i64<W: Write>(w: &mut W, value: i64) -> Result<()> {
    write_varint_u64(w, zigzag_encode(value))
}

pub fn read_varint_i64<R: Read>(r: &mut R, file: &'static str) -> Result<i64> {
    Ok(zigzag_decode(read_varint_u64(r, file)?))
}

/// Write a big-endian u16.
pub fn write_u16_be<W: Write>(w: &mut W, value: u16) -> Result<()> {
    w.write_all(&value.to_be_bytes())?;
    Ok(())
}

/// Write a big-endian u32.
pub fn write_u32_be<W: Write>(w: &mut W, value: u32) -> Result<()> {
    w.write_all(&value.to_be_bytes())?;
    Ok(())
}

/// Write a big-endian u64.
pub fn write_u64_be<W: Write>(w: &mut W, value: u64) -> Result<()> {
    w.write_all(&value.to_be_bytes())?;
    Ok(())
}

/// Write a big-endian i64.
pub fn write_i64_be<W: Write>(w: &mut W, value: i64) -> Result<()> {
    w.write_all(&value.to_be_bytes())?;
    Ok(())
}

/// Read a big-endian u16 from a byte slice.
pub fn read_u16_be(buf: &[u8]) -> u16 {
    u16::from_be_bytes([buf[0], buf[1]])
}

pub fn read_u32_be(buf: &[u8]) -> u32 {
    u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]])
}

pub fn read_u64_be(buf: &[u8]) -> u64 {
    u64::from_be_bytes([
        buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
    ])
}

pub fn read_i64_be(buf: &[u8]) -> i64 {
    i64::from_be_bytes([
        buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
    ])
}

/// Compute CRC32 (IEEE polynomial) over `bytes`. The trailer of every
/// binary file is the CRC of everything written before it.
pub fn crc32(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
}

/// Helper: read `n` bytes into a fresh `Vec`. Fails with `UnexpectedEof`
/// rather than returning a short read.
pub fn read_exact_n<R: Read>(r: &mut R, n: usize, file: &'static str) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            StorageError::UnexpectedEof { file }
        } else {
            StorageError::Io(e)
        }
    })?;
    Ok(buf)
}

/// Verify a 4-byte magic. The reader-side equivalent of writing a
/// constant `&[u8; 4]` literal at the start of a file.
pub fn expect_magic<R: Read>(
    r: &mut R,
    expected: &[u8; 4],
    file: &'static str,
) -> Result<()> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            StorageError::UnexpectedEof { file }
        } else {
            StorageError::Io(e)
        }
    })?;
    if &buf != expected {
        return Err(StorageError::BadMagic {
            file,
            expected: *expected,
            actual: buf,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn varint_u64_round_trip_small_values() {
        for v in [0u64, 1, 127, 128, 16_383, 16_384, 1_000_000] {
            let mut buf = Vec::new();
            write_varint_u64(&mut buf, v).unwrap();
            let mut cur = Cursor::new(&buf);
            assert_eq!(read_varint_u64(&mut cur, "test").unwrap(), v);
        }
    }

    #[test]
    fn varint_u64_round_trip_max() {
        let mut buf = Vec::new();
        write_varint_u64(&mut buf, u64::MAX).unwrap();
        let mut cur = Cursor::new(&buf);
        assert_eq!(read_varint_u64(&mut cur, "test").unwrap(), u64::MAX);
    }

    #[test]
    fn varint_u64_single_byte_for_under_128() {
        let mut buf = Vec::new();
        write_varint_u64(&mut buf, 127).unwrap();
        assert_eq!(buf, vec![127]);
    }

    #[test]
    fn varint_u64_two_bytes_for_128() {
        let mut buf = Vec::new();
        write_varint_u64(&mut buf, 128).unwrap();
        // 128 = 0b1000_0000 → low 7 bits = 0 (with continuation),
        // next byte = 1.
        assert_eq!(buf, vec![0x80, 0x01]);
    }

    #[test]
    fn varint_u64_too_wide_returns_error() {
        // 11 bytes all with continuation bit set — exceeds 64-bit width.
        let bytes = vec![0xFFu8; 11];
        let mut cur = Cursor::new(&bytes);
        let err = read_varint_u64(&mut cur, "test").unwrap_err();
        assert!(matches!(err, StorageError::Malformed { .. }));
    }

    #[test]
    fn varint_u64_short_returns_eof() {
        let bytes = vec![0x80u8];
        let mut cur = Cursor::new(&bytes);
        let err = read_varint_u64(&mut cur, "test").unwrap_err();
        assert!(matches!(err, StorageError::UnexpectedEof { .. }));
    }

    #[test]
    fn zigzag_round_trip() {
        for v in [0i64, 1, -1, 2, -2, 100, -100, i64::MAX, i64::MIN] {
            assert_eq!(zigzag_decode(zigzag_encode(v)), v, "v={v}");
        }
    }

    #[test]
    fn zigzag_small_signed_fit_in_one_byte() {
        // -1 → 1, -63 → 125, -64 → 127 (all single-byte varints).
        assert_eq!(zigzag_encode(-1), 1);
        assert_eq!(zigzag_encode(-64), 127);
        assert_eq!(zigzag_encode(63), 126);
        let mut buf = Vec::new();
        write_varint_i64(&mut buf, -1).unwrap();
        assert_eq!(buf, vec![1]);
    }

    #[test]
    fn fixed_width_round_trip() {
        let mut buf = Vec::new();
        write_u16_be(&mut buf, 0x1234).unwrap();
        write_u32_be(&mut buf, 0xDEADBEEF).unwrap();
        write_u64_be(&mut buf, 0x0102030405060708).unwrap();
        assert_eq!(read_u16_be(&buf[..2]), 0x1234);
        assert_eq!(read_u32_be(&buf[2..6]), 0xDEADBEEF);
        assert_eq!(read_u64_be(&buf[6..14]), 0x0102030405060708);
    }

    #[test]
    fn crc32_known_value() {
        // crc32("123456789") = 0xCBF43926 (canonical CRC-32/IEEE test vec)
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
    }

    #[test]
    fn expect_magic_succeeds_on_match() {
        let bytes = b"WWST";
        let mut cur = Cursor::new(bytes);
        expect_magic(&mut cur, b"WWST", "test").unwrap();
    }

    #[test]
    fn expect_magic_errs_on_mismatch() {
        let bytes = b"BAD!";
        let mut cur = Cursor::new(bytes);
        let err = expect_magic(&mut cur, b"WWST", "test").unwrap_err();
        match err {
            StorageError::BadMagic {
                expected, actual, ..
            } => {
                assert_eq!(&expected, b"WWST");
                assert_eq!(&actual, b"BAD!");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
