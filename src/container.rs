//! Per-bucket container holding the low 16 bits of values.
//!
//! Two representations:
//! - `Array`: sorted `Vec<u16>`, used while cardinality <= `ARRAY_LIMIT`.
//! - `Bitmap`: 2^16 bits (8192 bytes), used for dense buckets.
//!
//! Conversions happen automatically at the density threshold and never
//! change the logical contents of the set.

pub const ARRAY_LIMIT: usize = 4096;
const BITMAP_WORDS: usize = 1024; // 1024 * 64 = 65536 bits
pub const BITMAP_BYTES: usize = BITMAP_WORDS * 8;

#[derive(Clone, Debug)]
pub enum Container {
    Array(Vec<u16>),
    Bitmap(Box<[u64; BITMAP_WORDS]>),
}

pub enum ContainerIter<'a> {
    Array(std::slice::Iter<'a, u16>),
    Bitmap(BitmapIter<'a>),
}

pub struct BitmapIter<'a> {
    words: &'a [u64; BITMAP_WORDS],
    word_idx: usize,
    bits: u64,
}

impl<'a> Iterator for BitmapIter<'a> {
    type Item = u16;
    fn next(&mut self) -> Option<u16> {
        loop {
            if self.bits != 0 {
                let bit = self.bits.trailing_zeros() as u16;
                self.bits &= self.bits - 1;
                return Some((self.word_idx as u16) * 64 + bit);
            }
            if self.word_idx >= BITMAP_WORDS {
                return None;
            }
            // Advance to the next non-zero word.
            self.word_idx += 1;
            while self.word_idx < BITMAP_WORDS && self.words[self.word_idx] == 0 {
                self.word_idx += 1;
            }
            if self.word_idx < BITMAP_WORDS {
                self.bits = self.words[self.word_idx];
            }
        }
    }
}

impl<'a> Iterator for ContainerIter<'a> {
    type Item = u16;
    fn next(&mut self) -> Option<u16> {
        match self {
            ContainerIter::Array(it) => it.next().copied(),
            ContainerIter::Bitmap(it) => it.next(),
        }
    }
}

impl Container {
    pub fn new_array() -> Self {
        Container::Array(Vec::new())
    }

    pub fn cardinality(&self) -> usize {
        match self {
            Container::Array(v) => v.len(),
            Container::Bitmap(words) => words.iter().map(|w| w.count_ones() as usize).sum(),
        }
    }

    /// Bytes used by this container's value storage (excluding struct header).
    pub fn stored_bytes(&self) -> usize {
        match self {
            Container::Array(v) => v.len() * 2,
            Container::Bitmap(_) => BITMAP_BYTES,
        }
    }

    pub fn contains(&self, v: u16) -> bool {
        match self {
            Container::Array(vec) => vec.binary_search(&v).is_ok(),
            Container::Bitmap(words) => {
                (words[(v / 64) as usize] >> (v % 64)) & 1 == 1
            }
        }
    }

    /// Insert; returns true if the value was not already present.
    pub fn add(&mut self, v: u16) -> bool {
        match self {
            Container::Array(vec) => match vec.binary_search(&v) {
                Ok(_) => false,
                Err(pos) => {
                    vec.insert(pos, v);
                    if vec.len() > ARRAY_LIMIT {
                        self.convert_to_bitmap();
                    }
                    true
                }
            },
            Container::Bitmap(words) => {
                let word = &mut words[(v / 64) as usize];
                let mask = 1u64 << (v % 64);
                let was = *word & mask != 0;
                *word |= mask;
                !was
            }
        }
    }

    /// Remove; returns true if the value was present.
    pub fn remove(&mut self, v: u16) -> bool {
        match self {
            Container::Array(vec) => match vec.binary_search(&v) {
                Ok(pos) => {
                    vec.remove(pos);
                    true
                }
                Err(_) => false,
            },
            Container::Bitmap(words) => {
                let word = &mut words[(v / 64) as usize];
                let mask = 1u64 << (v % 64);
                if *word & mask == 0 {
                    return false;
                }
                *word &= !mask;
                if words.iter().map(|w| w.count_ones() as usize).sum::<usize>() <= ARRAY_LIMIT
                {
                    self.convert_to_array();
                }
                true
            }
        }
    }

    fn convert_to_bitmap(&mut self) {
        if let Container::Array(vec) = self {
            let mut words = Box::new([0u64; BITMAP_WORDS]);
            for &v in vec.iter() {
                words[(v / 64) as usize] |= 1u64 << (v % 64);
            }
            *self = Container::Bitmap(words);
        }
    }

    fn convert_to_array(&mut self) {
        if let Container::Bitmap(_) = self {
            let vec: Vec<u16> = self.iter().collect();
            *self = Container::Array(vec);
        }
    }

    pub fn iter(&self) -> ContainerIter<'_> {
        match self {
            Container::Array(vec) => ContainerIter::Array(vec.iter()),
            Container::Bitmap(words) => ContainerIter::Bitmap(BitmapIter {
                words,
                word_idx: 0,
                bits: words[0],
            }),
        }
    }

    /// Pick the most compact representation for a container of `card` values
    /// built by a set operation.
    fn from_sorted(card_hint: usize, values: impl Iterator<Item = u16>) -> Self {
        if card_hint <= ARRAY_LIMIT {
            Container::Array(values.collect())
        } else {
            let mut words = Box::new([0u64; BITMAP_WORDS]);
            for v in values {
                words[(v / 64) as usize] |= 1u64 << (v % 64);
            }
            Container::Bitmap(words)
        }
    }

    pub fn union(&self, other: &Container) -> Container {
        match (self, other) {
            (Container::Bitmap(a), Container::Bitmap(b)) => {
                let mut words = Box::new([0u64; BITMAP_WORDS]);
                for i in 0..BITMAP_WORDS {
                    words[i] = a[i] | b[i];
                }
                Container::Bitmap(words)
            }
            _ => {
                let card = self.cardinality() + other.cardinality(); // upper bound
                Container::from_sorted(card, MergeIter::new_union(self.iter(), other.iter()))
            }
        }
    }

    pub fn intersect(&self, other: &Container) -> Container {
        match (self, other) {
            (Container::Bitmap(a), Container::Bitmap(b)) => {
                let mut words = Box::new([0u64; BITMAP_WORDS]);
                let mut card = 0usize;
                for i in 0..BITMAP_WORDS {
                    let w = a[i] & b[i];
                    card += w.count_ones() as usize;
                    words[i] = w;
                }
                if card <= ARRAY_LIMIT {
                    let tmp = Container::Bitmap(words);
                    Container::Array(tmp.iter().collect())
                } else {
                    Container::Bitmap(words)
                }
            }
            _ => {
                let card = self.cardinality().min(other.cardinality());
                Container::from_sorted(card, MergeIter::new_intersect(self.iter(), other.iter()))
            }
        }
    }

    pub fn difference(&self, other: &Container) -> Container {
        let card = self.cardinality();
        Container::from_sorted(card, MergeIter::new_difference(self.iter(), other.iter()))
    }

    pub fn xor(&self, other: &Container) -> Container {
        match (self, other) {
            (Container::Bitmap(a), Container::Bitmap(b)) => {
                let mut words = Box::new([0u64; BITMAP_WORDS]);
                let mut card = 0usize;
                for i in 0..BITMAP_WORDS {
                    let w = a[i] ^ b[i];
                    card += w.count_ones() as usize;
                    words[i] = w;
                }
                if card <= ARRAY_LIMIT {
                    let tmp = Container::Bitmap(words);
                    Container::Array(tmp.iter().collect())
                } else {
                    Container::Bitmap(words)
                }
            }
            _ => {
                let card = self.cardinality() + other.cardinality();
                Container::from_sorted(card, MergeIter::new_xor(self.iter(), other.iter()))
            }
        }
    }
}

/// Merge iterators over two sorted u16 streams implementing set operations.
enum MergeIter<'a> {
    Union(Peek2<'a>),
    Intersect(Peek2<'a>),
    Difference(Peek2<'a>),
    Xor(Peek2<'a>),
}

struct Peek2<'a> {
    a: std::iter::Peekable<ContainerIter<'a>>,
    b: std::iter::Peekable<ContainerIter<'a>>,
}

impl<'a> MergeIter<'a> {
    fn new_union(a: ContainerIter<'a>, b: ContainerIter<'a>) -> Self {
        MergeIter::Union(Peek2 { a: a.peekable(), b: b.peekable() })
    }
    fn new_intersect(a: ContainerIter<'a>, b: ContainerIter<'a>) -> Self {
        MergeIter::Intersect(Peek2 { a: a.peekable(), b: b.peekable() })
    }
    fn new_difference(a: ContainerIter<'a>, b: ContainerIter<'a>) -> Self {
        MergeIter::Difference(Peek2 { a: a.peekable(), b: b.peekable() })
    }
    fn new_xor(a: ContainerIter<'a>, b: ContainerIter<'a>) -> Self {
        MergeIter::Xor(Peek2 { a: a.peekable(), b: b.peekable() })
    }
}

impl<'a> Iterator for MergeIter<'a> {
    type Item = u16;
    fn next(&mut self) -> Option<u16> {
        match self {
            MergeIter::Union(p) => match (p.a.peek(), p.b.peek()) {
                (Some(&x), Some(&y)) => {
                    if x <= y {
                        p.a.next();
                    }
                    if y <= x {
                        p.b.next();
                    }
                    Some(x.min(y))
                }
                (Some(&x), None) => {
                    p.a.next();
                    Some(x)
                }
                (None, Some(&y)) => {
                    p.b.next();
                    Some(y)
                }
                (None, None) => None,
            },
            MergeIter::Intersect(p) => loop {
                match (p.a.peek(), p.b.peek()) {
                    (Some(&x), Some(&y)) => {
                        if x < y {
                            p.a.next();
                        } else if y < x {
                            p.b.next();
                        } else {
                            p.a.next();
                            p.b.next();
                            return Some(x);
                        }
                    }
                    _ => return None,
                }
            },
            MergeIter::Difference(p) => loop {
                match (p.a.peek(), p.b.peek()) {
                    (Some(&x), Some(&y)) => {
                        if x < y {
                            p.a.next();
                            return Some(x);
                        } else if y < x {
                            p.b.next();
                        } else {
                            p.a.next();
                            p.b.next();
                        }
                    }
                    (Some(&x), None) => {
                        p.a.next();
                        return Some(x);
                    }
                    _ => return None,
                }
            },
            MergeIter::Xor(p) => loop {
                match (p.a.peek(), p.b.peek()) {
                    (Some(&x), Some(&y)) => {
                        if x < y {
                            p.a.next();
                            return Some(x);
                        } else if y < x {
                            p.b.next();
                            return Some(y);
                        } else {
                            p.a.next();
                            p.b.next();
                        }
                    }
                    (Some(&x), None) => {
                        p.a.next();
                        return Some(x);
                    }
                    (None, Some(&y)) => {
                        p.b.next();
                        return Some(y);
                    }
                    (None, None) => return None,
                }
            },
        }
    }
}
