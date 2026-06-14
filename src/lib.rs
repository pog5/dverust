//! Faithful Rust port of LibDumbVersion (DumbVersion .dvp format).
//! Byte-format compatible with the original C# NativeAOT tools.

pub mod fastcdc;

use std::io::{self, Read, Write};

pub const MAGIC: &[u8] = b"DUMBVER\x01";

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cmd {
    Copy = 1,
    Insert = 2,
    Eof = 3,
    CopyTarget = 4,
}

#[inline]
pub fn encode_zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
pub fn decode_zigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// LEB128 unsigned varint (matches .NET Write7BitEncodedInt64).
#[inline]
pub fn write_varint<W: Write>(w: &mut W, value: u64) -> io::Result<()> {
    let mut v = value;
    let mut buf = [0u8; 10];
    let mut n = 0;
    while v >= 0x80 {
        buf[n] = (v as u8) | 0x80;
        n += 1;
        v >>= 7;
    }
    buf[n] = v as u8;
    n += 1;
    w.write_all(&buf[..n])
}

/// Read LEB128 varint from a byte cursor.
#[inline]
pub fn read_varint(buf: &[u8], pos: &mut usize) -> io::Result<u64> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        let b = *buf
            .get(*pos)
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "varint eof"))?;
        *pos += 1;
        result |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    Ok(result)
}

/// Patch file header.
pub struct Header {
    pub target_size: i64,
    pub base_hash: [u8; 32],
    pub target_hash: [u8; 32],
    pub base_filename: String,
}

impl Header {
    pub fn size(&self) -> usize {
        MAGIC.len() + 8 + 32 + 32 + 2 + self.base_filename.as_bytes().len()
    }

    pub fn write<W: Write>(&self, w: &mut W) -> io::Result<()> {
        w.write_all(MAGIC)?;
        w.write_all(&self.target_size.to_le_bytes())?;
        w.write_all(&self.base_hash)?;
        w.write_all(&self.target_hash)?;
        let fb = self.base_filename.as_bytes();
        w.write_all(&(fb.len() as u16).to_le_bytes())?;
        w.write_all(fb)?;
        Ok(())
    }

    pub fn read<R: Read>(r: &mut R) -> io::Result<Header> {
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        if magic != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid patch file."));
        }
        let mut sz = [0u8; 8];
        r.read_exact(&mut sz)?;
        let target_size = i64::from_le_bytes(sz);
        if target_size < 0 || target_size > 250i64 * 1024 * 1024 * 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid target size"));
        }
        let mut base_hash = [0u8; 32];
        let mut target_hash = [0u8; 32];
        r.read_exact(&mut base_hash)?;
        r.read_exact(&mut target_hash)?;
        let mut fnlen = [0u8; 2];
        r.read_exact(&mut fnlen)?;
        let fnlen = u16::from_le_bytes(fnlen) as usize;
        let mut fnbuf = vec![0u8; fnlen];
        r.read_exact(&mut fnbuf)?;
        let base_filename = String::from_utf8_lossy(&fnbuf).into_owned();
        Ok(Header { target_size, base_hash, target_hash, base_filename })
    }
}
