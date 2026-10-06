//! Binary serialization with version tag and CRC-32 integrity check.
//!
//! Layout (all integers little-endian):
//!   magic      [u8; 4]  = "BIDX"
//!   version    u32      = FORMAT_VERSION
//!   buckets    u32      = number of containers
//!   per container:
//!     key        u16    = high 16 bits
//!     kind       u8     = 0 array, 1 bitmap
//!     card       u32    = cardinality
//!     payload           = array: card * u16 (strictly ascending)
//!                         bitmap: 1024 * u64
//!   crc32      u32      = CRC-32 of every byte before this field

use crate::container::{Container, BITMAP_BYTES};
use crate::crc32::crc32;
use crate::{BitmapIndex, FORMAT_VERSION};
use std::fmt;
use std::io::{self, Read, Write};
use std::path::Path;

const MAGIC: &[u8; 4] = b"BIDX";
const KIND_ARRAY: u8 = 0;
const KIND_BITMAP: u8 = 1;

#[derive(Debug)]
pub enum SerError {
    Io(io::Error),
    BadMagic,
    UnsupportedVersion(u32),
    Truncated,
    CrcMismatch { expected: u32, actual: u32 },
    InvalidStructure(&'static str),
}

impl fmt::Display for SerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SerError::Io(e) => write!(f, "io error: {e}"),
            SerError::BadMagic => write!(f, "bad magic header"),
            SerError::UnsupportedVersion(v) => write!(f, "unsupported version {v}"),
            SerError::Truncated => write!(f, "truncated input"),
            SerError::CrcMismatch { expected, actual } => {
                write!(f, "crc mismatch: expected {expected:#010x}, got {actual:#010x}")
            }
            SerError::InvalidStructure(why) => write!(f, "invalid structure: {why}"),
        }
    }
}

impl std::error::Error for SerError {}

impl From<io::Error> for SerError {
    fn from(e: io::Error) -> Self {
        SerError::Io(e)
    }
}

impl BitmapIndex {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        buf.extend_from_slice(&(self.buckets.len() as u32).to_le_bytes());
        for (&key, c) in &self.buckets {
            buf.extend_from_slice(&key.to_le_bytes());
            match c {
                Container::Array(vec) => {
                    buf.push(KIND_ARRAY);
                    buf.extend_from_slice(&(vec.len() as u32).to_le_bytes());
                    for v in vec {
                        buf.extend_from_slice(&v.to_le_bytes());
                    }
                }
                Container::Bitmap(words) => {
                    buf.push(KIND_BITMAP);
                    buf.extend_from_slice(&(words.iter().map(|w| w.count_ones() as u64).sum::<u64>() as u32).to_le_bytes());
                    for w in words.iter() {
                        buf.extend_from_slice(&w.to_le_bytes());
                    }
                }
            }
        }
        let crc = crc32(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());
        buf
    }

    pub fn save<P: AsRef<Path>>(&self, path: P) -> Result<(), SerError> {
        let mut f = std::fs::File::create(path)?;
        f.write_all(&self.to_bytes())?;
        Ok(())
    }

    pub fn load<P: AsRef<Path>>(path: P) -> Result<BitmapIndex, SerError> {
        let mut data = Vec::new();
        std::fs::File::open(path)?.read_to_end(&mut data)?;
        Self::from_bytes(&data)
    }

    pub fn from_bytes(data: &[u8]) -> Result<BitmapIndex, SerError> {
        if data.len() < 4 + 4 + 4 + 4 {
            return Err(SerError::Truncated);
        }
        // CRC check first: any corruption anywhere is detected here.
        let (body, crc_field) = data.split_at(data.len() - 4);
        let expected = u32::from_le_bytes(crc_field.try_into().unwrap());
        let actual = crc32(body);
        if expected != actual {
            return Err(SerError::CrcMismatch { expected, actual });
        }
        let mut r = Reader { buf: body, pos: 0 };
        if r.take(4)? != MAGIC {
            return Err(SerError::BadMagic);
        }
        let version = r.u32()?;
        if version != FORMAT_VERSION {
            return Err(SerError::UnsupportedVersion(version));
        }
        let n_buckets = r.u32()?;
        let mut out = BitmapIndex::new();
        let mut prev_key: Option<u16> = None;
        for _ in 0..n_buckets {
            let key = r.u16()?;
            if let Some(p) = prev_key {
                if key <= p {
                    return Err(SerError::InvalidStructure("bucket keys not strictly ascending"));
                }
            }
            prev_key = Some(key);
            let kind = r.u8()?;
            let card = r.u32()? as usize;
            let container = match kind {
                KIND_ARRAY => {
                    if card > 65536 {
                        return Err(SerError::InvalidStructure("array cardinality too large"));
                    }
                    let mut vec = Vec::with_capacity(card);
                    let mut prev: Option<u16> = None;
                    for _ in 0..card {
                        let v = r.u16()?;
                        if let Some(p) = prev {
                            if v <= p {
                                return Err(SerError::InvalidStructure("array not strictly ascending"));
                            }
                        }
                        prev = Some(v);
                        vec.push(v);
                    }
                    Container::Array(vec)
                }
                KIND_BITMAP => {
                    let mut words = Box::new([0u64; 1024]);
                    let mut actual_card = 0usize;
                    for w in words.iter_mut() {
                        *w = r.u64()?;
                        actual_card += w.count_ones() as usize;
                    }
                    if actual_card != card {
                        return Err(SerError::InvalidStructure("bitmap cardinality mismatch"));
                    }
                    Container::Bitmap(words)
                }
                _ => return Err(SerError::InvalidStructure("unknown container kind")),
            };
            let real_card = container.cardinality();
            if real_card == 0 {
                return Err(SerError::InvalidStructure("empty container"));
            }
            out.len += real_card as u64;
            out.buckets.insert(key, container);
        }
        if r.pos != body.len() {
            return Err(SerError::InvalidStructure("trailing bytes"));
        }
        Ok(out)
    }

    /// Size of the serialized form in bytes (what a file would occupy).
    pub fn serialized_size(&self) -> u64 {
        let mut n = 4 + 4 + 4 + 4; // magic + version + count + crc
        for c in self.buckets.values() {
            n += 2 + 1 + 4;
            n += match c {
                Container::Array(v) => v.len() * 2,
                Container::Bitmap(_) => BITMAP_BYTES,
            };
        }
        n as u64
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], SerError> {
        if self.pos + n > self.buf.len() {
            return Err(SerError::Truncated);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, SerError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, SerError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, SerError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, SerError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}
