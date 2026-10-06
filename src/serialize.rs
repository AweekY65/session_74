//! Binary serialization with versioning and integrity checking.
//!
//! Layout (all integers little-endian):
//! ```text
//! magic      : 4 bytes  "RBMP"
//! version    : u32      currently 1
//! containers : u32      number of containers
//! per container:
//!   key         : u16   high-16-bit bucket key
//!   kind        : u8    0 = array, 1 = bitmap
//!   cardinality : u32
//!   payload     : array -> cardinality * u16
//!                 bitmap -> 1024 * u64
//! crc32      : u32      CRC-32 (IEEE) of every preceding byte
//! ```

use crate::container::{Container, BITMAP_WORDS};
use crate::BitmapIndex;
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

const MAGIC: &[u8; 4] = b"RBMP";
const VERSION: u32 = 1;
const KIND_ARRAY: u8 = 0;
const KIND_BITMAP: u8 = 1;

/// Errors produced when loading a serialized index.
#[derive(Debug)]
pub enum DeserializeError {
    Io(io::Error),
    BadMagic,
    UnsupportedVersion(u32),
    Truncated { offset: usize, needed: usize, len: usize },
    InvalidContainerKind(u8),
    CardinalityMismatch { expected: u32, actual: u32 },
    CrcMismatch { expected: u32, actual: u32 },
    TrailingBytes(usize),
}

impl fmt::Display for DeserializeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeserializeError::Io(e) => write!(f, "I/O error: {e}"),
            DeserializeError::BadMagic => write!(f, "bad magic header"),
            DeserializeError::UnsupportedVersion(v) => {
                write!(f, "unsupported format version {v}")
            }
            DeserializeError::Truncated { offset, needed, len } => write!(
                f,
                "truncated input: need {needed} bytes at offset {offset}, buffer has {len}"
            ),
            DeserializeError::InvalidContainerKind(k) => {
                write!(f, "invalid container kind {k}")
            }
            DeserializeError::CardinalityMismatch { expected, actual } => write!(
                f,
                "container cardinality mismatch: header says {expected}, payload has {actual}"
            ),
            DeserializeError::CrcMismatch { expected, actual } => write!(
                f,
                "CRC mismatch: expected {expected:#010x}, computed {actual:#010x}"
            ),
            DeserializeError::TrailingBytes(n) => write!(f, "{n} trailing bytes after checksum"),
        }
    }
}

impl std::error::Error for DeserializeError {}

impl From<io::Error> for DeserializeError {
    fn from(e: io::Error) -> Self {
        DeserializeError::Io(e)
    }
}

/// CRC-32 (IEEE 802.3, polynomial 0xEDB88320), table-driven.
struct Crc32 {
    table: [u32; 256],
}

impl Crc32 {
    fn new() -> Self {
        let mut table = [0u32; 256];
        for (i, entry) in table.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *entry = c;
        }
        Crc32 { table }
    }

    fn checksum(&self, data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &byte in data {
            crc = self.table[((crc ^ byte as u32) & 0xFF) as usize] ^ (crc >> 8);
        }
        crc ^ 0xFFFF_FFFF
    }
}

impl BitmapIndex {
    /// Serialize the index into a byte vector.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&(self.containers.len() as u32).to_le_bytes());
        for (key, container) in &self.containers {
            out.extend_from_slice(&key.to_le_bytes());
            match container {
                Container::Array(values) => {
                    out.push(KIND_ARRAY);
                    out.extend_from_slice(&(values.len() as u32).to_le_bytes());
                    for &v in values {
                        out.extend_from_slice(&v.to_le_bytes());
                    }
                }
                Container::Bitmap { words, cardinality } => {
                    out.push(KIND_BITMAP);
                    out.extend_from_slice(&cardinality.to_le_bytes());
                    for &w in words.iter() {
                        out.extend_from_slice(&w.to_le_bytes());
                    }
                }
            }
        }
        let crc = Crc32::new().checksum(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    /// Serialize the index and write it to a local file.
    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        fs::write(path, self.to_bytes())
    }

    /// Deserialize an index from bytes, validating header, structure and
    /// checksum. Any corruption is reported as an error.
    pub fn from_bytes(data: &[u8]) -> Result<BitmapIndex, DeserializeError> {
        let mut reader = Reader { data, pos: 0 };

        let magic = reader.take(4)?;
        if magic != MAGIC {
            return Err(DeserializeError::BadMagic);
        }
        let version = reader.u32()?;
        if version != VERSION {
            return Err(DeserializeError::UnsupportedVersion(version));
        }
        let container_count = reader.u32()? as usize;

        let mut containers: BTreeMap<u16, Container> = BTreeMap::new();
        for _ in 0..container_count {
            let key = reader.u16()?;
            let kind = reader.u8()?;
            let cardinality = reader.u32()?;
            let container = match kind {
                KIND_ARRAY => {
                    let mut values = Vec::with_capacity(cardinality as usize);
                    for _ in 0..cardinality {
                        values.push(reader.u16()?);
                    }
                    // Structural validation: sorted and deduplicated.
                    if values.windows(2).any(|w| w[0] >= w[1]) {
                        return Err(DeserializeError::CardinalityMismatch {
                            expected: cardinality,
                            actual: 0,
                        });
                    }
                    Container::Array(values)
                }
                KIND_BITMAP => {
                    let mut words = Box::new([0u64; BITMAP_WORDS]);
                    let mut actual = 0u32;
                    for word in words.iter_mut() {
                        *word = reader.u64()?;
                        actual += word.count_ones();
                    }
                    if actual != cardinality {
                        return Err(DeserializeError::CardinalityMismatch {
                            expected: cardinality,
                            actual,
                        });
                    }
                    Container::Bitmap { words, cardinality }
                }
                other => return Err(DeserializeError::InvalidContainerKind(other)),
            };
            containers.insert(key, container);
        }

        let expected_crc = reader.u32()?;
        let payload_len = reader.pos - 4;
        let actual_crc = Crc32::new().checksum(&data[..payload_len]);
        if expected_crc != actual_crc {
            return Err(DeserializeError::CrcMismatch {
                expected: expected_crc,
                actual: actual_crc,
            });
        }
        if reader.pos != data.len() {
            return Err(DeserializeError::TrailingBytes(data.len() - reader.pos));
        }

        Ok(BitmapIndex { containers })
    }

    /// Load an index from a local file, validating integrity.
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<BitmapIndex, DeserializeError> {
        let data = fs::read(path)?;
        BitmapIndex::from_bytes(&data)
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DeserializeError> {
        if self.pos + n > self.data.len() {
            return Err(DeserializeError::Truncated {
                offset: self.pos,
                needed: n,
                len: self.data.len(),
            });
        }
        let slice = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, DeserializeError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, DeserializeError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, DeserializeError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, DeserializeError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}
