//! A local compressed integer bitmap index ("roaring-style") for u32 sets.
//!
//! Values are bucketed by their high 16 bits. Each bucket is a `Container`
//! that is either a sorted array (sparse) or a 65536-bit bitmap (dense),
//! chosen automatically by density. All data lives in memory or in local
//! files; no database, no external bitmap library, no network service.

mod container;
mod crc32;
mod ser;

pub use container::{Container, ARRAY_LIMIT};

use std::collections::BTreeMap;

pub const FORMAT_VERSION: u32 = 1;

/// A compressed set of non-negative 32-bit integers.
#[derive(Clone, Debug, Default)]
pub struct BitmapIndex {
    /// high 16 bits -> container of low 16 bits
    buckets: BTreeMap<u16, Container>,
    len: u64,
}

impl BitmapIndex {
    pub fn new() -> Self {
        BitmapIndex {
            buckets: BTreeMap::new(),
            len: 0,
        }
    }

    #[inline]
    fn split(v: u32) -> (u16, u16) {
        ((v >> 16) as u16, (v & 0xFFFF) as u16)
    }

    pub fn add(&mut self, v: u32) -> bool {
        let (hi, lo) = Self::split(v);
        let c = self.buckets.entry(hi).or_insert_with(Container::new_array);
        if c.add(lo) {
            self.len += 1;
            true
        } else {
            false
        }
    }

    pub fn remove(&mut self, v: u32) -> bool {
        let (hi, lo) = Self::split(v);
        if let Some(c) = self.buckets.get_mut(&hi) {
            if c.remove(lo) {
                self.len -= 1;
                if c.cardinality() == 0 {
                    self.buckets.remove(&hi);
                }
                return true;
            }
        }
        false
    }

    pub fn contains(&self, v: u32) -> bool {
        let (hi, lo) = Self::split(v);
        self.buckets.get(&hi).map_or(false, |c| c.contains(lo))
    }

    pub fn cardinality(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Total bytes used by container value storage (arrays + bitmaps),
    /// excluding map/struct overhead.
    pub fn stored_bytes(&self) -> u64 {
        self.buckets.values().map(|c| c.stored_bytes() as u64).sum()
    }

    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// Ordered (ascending) iteration over all values.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.buckets.iter().flat_map(|(&hi, c)| {
            let base = (hi as u32) << 16;
            c.iter().map(move |lo| base | lo as u32)
        })
    }

    fn combine(&self, other: &BitmapIndex, op: impl Fn(&Container, &Container) -> Container) -> BitmapIndex {
        let mut out = BitmapIndex::new();
        for (&k, a) in &self.buckets {
            if let Some(b) = other.buckets.get(&k) {
                let c = op(a, b);
                let card = c.cardinality();
                if card > 0 {
                    out.len += card as u64;
                    out.buckets.insert(k, c);
                }
            }
        }
        out
    }

    pub fn intersection(&self, other: &BitmapIndex) -> BitmapIndex {
        // Iterate the smaller map for efficiency.
        if self.buckets.len() <= other.buckets.len() {
            self.combine(other, Container::intersect)
        } else {
            other.combine(self, Container::intersect)
        }
    }

    pub fn union(&self, other: &BitmapIndex) -> BitmapIndex {
        let mut out = BitmapIndex::new();
        let mut it_a = self.buckets.iter().peekable();
        let mut it_b = other.buckets.iter().peekable();
        loop {
            match (it_a.peek(), it_b.peek()) {
                (Some((&ka, a)), Some((&kb, b))) => {
                    if ka < kb {
                        out.insert_bucket(ka, (*a).clone());
                        it_a.next();
                    } else if kb < ka {
                        out.insert_bucket(kb, (*b).clone());
                        it_b.next();
                    } else {
                        out.insert_bucket(ka, a.union(b));
                        it_a.next();
                        it_b.next();
                    }
                }
                (Some((&ka, a)), None) => {
                    out.insert_bucket(ka, (*a).clone());
                    it_a.next();
                }
                (None, Some((&kb, b))) => {
                    out.insert_bucket(kb, (*b).clone());
                    it_b.next();
                }
                (None, None) => break,
            }
        }
        out
    }

    pub fn difference(&self, other: &BitmapIndex) -> BitmapIndex {
        let mut out = BitmapIndex::new();
        for (&k, a) in &self.buckets {
            match other.buckets.get(&k) {
                Some(b) => {
                    let c = a.difference(b);
                    let card = c.cardinality();
                    if card > 0 {
                        out.len += card as u64;
                        out.buckets.insert(k, c);
                    }
                }
                None => out.insert_bucket(k, a.clone()),
            }
        }
        out
    }

    pub fn xor(&self, other: &BitmapIndex) -> BitmapIndex {
        let mut out = BitmapIndex::new();
        let mut it_a = self.buckets.iter().peekable();
        let mut it_b = other.buckets.iter().peekable();
        loop {
            match (it_a.peek(), it_b.peek()) {
                (Some((&ka, a)), Some((&kb, b))) => {
                    if ka < kb {
                        out.insert_bucket(ka, (*a).clone());
                        it_a.next();
                    } else if kb < ka {
                        out.insert_bucket(kb, (*b).clone());
                        it_b.next();
                    } else {
                        let c = a.xor(b);
                        let card = c.cardinality();
                        if card > 0 {
                            out.len += card as u64;
                            out.buckets.insert(ka, c);
                        }
                        it_a.next();
                        it_b.next();
                    }
                }
                (Some((&ka, a)), None) => {
                    out.insert_bucket(ka, (*a).clone());
                    it_a.next();
                }
                (None, Some((&kb, b))) => {
                    out.insert_bucket(kb, (*b).clone());
                    it_b.next();
                }
                (None, None) => break,
            }
        }
        out
    }

    fn insert_bucket(&mut self, key: u16, c: Container) {
        let card = c.cardinality();
        if card > 0 {
            self.len += card as u64;
            self.buckets.insert(key, c);
        }
    }
}
