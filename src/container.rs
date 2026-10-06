//! Container representations for the low 16 bits of values sharing a common
//! high-16-bit key.

/// Maximum cardinality of an array container before it is converted to a
/// bitmap container. At 4096 entries both representations use 8 KiB of
/// payload, so this is the natural density break-even point.
pub const ARRAY_TO_BITMAP_THRESHOLD: usize = 4096;

/// Number of 64-bit words in a bitmap container (65536 bits).
pub const BITMAP_WORDS: usize = 1024;

/// A container holding the low 16 bits of a set of values.
#[derive(Clone, Debug)]
pub enum Container {
    /// Sorted vector of low-16-bit values. Used for sparse containers.
    Array(Vec<u16>),
    /// Fixed 65536-bit bitmap plus cached cardinality. Used for dense
    /// containers.
    Bitmap { words: Box<[u64; BITMAP_WORDS]>, cardinality: u32 },
}

impl Container {
    pub fn empty_array() -> Container {
        Container::Array(Vec::new())
    }

    pub fn cardinality(&self) -> u32 {
        match self {
            Container::Array(values) => values.len() as u32,
            Container::Bitmap { cardinality, .. } => *cardinality,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.cardinality() == 0
    }

    /// Payload bytes used by this container (excluding enum bookkeeping).
    pub fn storage_bytes(&self) -> usize {
        match self {
            Container::Array(values) => values.len() * 2,
            Container::Bitmap { .. } => BITMAP_WORDS * 8,
        }
    }

    pub fn contains(&self, low: u16) -> bool {
        match self {
            Container::Array(values) => values.binary_search(&low).is_ok(),
            Container::Bitmap { words, .. } => {
                (words[(low >> 6) as usize] >> (low & 63)) & 1 == 1
            }
        }
    }

    /// Insert `low`; returns `true` if newly added. May convert the
    /// representation when the density threshold is crossed.
    pub fn add(&mut self, low: u16) -> bool {
        match self {
            Container::Array(values) => match values.binary_search(&low) {
                Ok(_) => false,
                Err(pos) => {
                    values.insert(pos, low);
                    if values.len() > ARRAY_TO_BITMAP_THRESHOLD {
                        self.convert_to_bitmap();
                    }
                    true
                }
            },
            Container::Bitmap { words, cardinality } => {
                let word = &mut words[(low >> 6) as usize];
                let mask = 1u64 << (low & 63);
                if *word & mask != 0 {
                    false
                } else {
                    *word |= mask;
                    *cardinality += 1;
                    true
                }
            }
        }
    }

    /// Remove `low`; returns `true` if it was present. May convert the
    /// representation when the density threshold is crossed.
    pub fn remove(&mut self, low: u16) -> bool {
        match self {
            Container::Array(values) => match values.binary_search(&low) {
                Ok(pos) => {
                    values.remove(pos);
                    true
                }
                Err(_) => false,
            },
            Container::Bitmap { words, cardinality } => {
                let word = &mut words[(low >> 6) as usize];
                let mask = 1u64 << (low & 63);
                if *word & mask == 0 {
                    false
                } else {
                    *word &= !mask;
                    *cardinality -= 1;
                    if *cardinality as usize <= ARRAY_TO_BITMAP_THRESHOLD {
                        self.convert_to_array();
                    }
                    true
                }
            }
        }
    }

    fn convert_to_bitmap(&mut self) {
        let Container::Array(values) = self else {
            return;
        };
        let mut words = Box::new([0u64; BITMAP_WORDS]);
        for &low in values.iter() {
            words[(low >> 6) as usize] |= 1u64 << (low & 63);
        }
        let cardinality = values.len() as u32;
        *self = Container::Bitmap { words, cardinality };
    }

    fn convert_to_array(&mut self) {
        let Container::Bitmap { words, cardinality } = self else {
            return;
        };
        let mut values = Vec::with_capacity(*cardinality as usize);
        for (word_index, &word) in words.iter().enumerate() {
            let mut bits = word;
            while bits != 0 {
                let bit = bits.trailing_zeros() as u16;
                values.push(((word_index as u16) << 6) | bit);
                bits &= bits - 1;
            }
        }
        *self = Container::Array(values);
    }

    /// Convert to bitmap representation regardless of density (used when a
    /// bitmap operand makes the result dense).
    fn ensure_bitmap(&mut self) {
        if matches!(self, Container::Array(_)) {
            self.convert_to_bitmap();
        }
    }

    /// After a bulk operation, pick the cheaper representation for the
    /// current cardinality.
    fn normalize(&mut self) {
        if let Container::Bitmap { cardinality, .. } = self {
            if *cardinality as usize <= ARRAY_TO_BITMAP_THRESHOLD {
                self.convert_to_array();
            }
        }
    }

    pub fn iter(&self) -> ContainerIter<'_> {
        match self {
            Container::Array(values) => ContainerIter::Array(values.iter()),
            Container::Bitmap { words, .. } => ContainerIter::Bitmap {
                words,
                word_index: 0,
                bits: words.first().copied().unwrap_or(0),
            },
        }
    }

    pub fn union(&self, other: &Container) -> Container {
        match (self, other) {
            (Container::Array(a), Container::Array(b)) => {
                Container::from_sorted_array(merge_union(a, b))
            }
            _ => {
                let mut result = self.clone();
                result.ensure_bitmap();
                if let Container::Bitmap { words, cardinality } = &mut result {
                    match other {
                        Container::Bitmap { words: other_words, .. } => {
                            let mut count = 0u32;
                            for (w, &o) in words.iter_mut().zip(other_words.iter()) {
                                *w |= o;
                                count += w.count_ones();
                            }
                            *cardinality = count;
                        }
                        Container::Array(values) => {
                            for &low in values {
                                let word = &mut words[(low >> 6) as usize];
                                let mask = 1u64 << (low & 63);
                                if *word & mask == 0 {
                                    *word |= mask;
                                    *cardinality += 1;
                                }
                            }
                        }
                    }
                }
                result.normalize();
                result
            }
        }
    }

    pub fn intersection(&self, other: &Container) -> Container {
        match (self, other) {
            (Container::Array(a), Container::Array(b)) => {
                Container::Array(merge_intersection(a, b))
            }
            (Container::Array(a), Container::Bitmap { words, .. })
            | (Container::Bitmap { words, .. }, Container::Array(a)) => {
                let filtered: Vec<u16> = a
                    .iter()
                    .copied()
                    .filter(|&low| (words[(low >> 6) as usize] >> (low & 63)) & 1 == 1)
                    .collect();
                Container::Array(filtered)
            }
            (
                Container::Bitmap { words: a, .. },
                Container::Bitmap { words: b, .. },
            ) => {
                let mut words = Box::new([0u64; BITMAP_WORDS]);
                let mut cardinality = 0u32;
                for i in 0..BITMAP_WORDS {
                    let w = a[i] & b[i];
                    words[i] = w;
                    cardinality += w.count_ones();
                }
                let mut result = Container::Bitmap { words, cardinality };
                result.normalize();
                result
            }
        }
    }

    pub fn difference(&self, other: &Container) -> Container {
        match (self, other) {
            (Container::Array(a), Container::Array(b)) => {
                Container::Array(merge_difference(a, b))
            }
            (Container::Array(a), Container::Bitmap { words, .. }) => {
                let filtered: Vec<u16> = a
                    .iter()
                    .copied()
                    .filter(|&low| (words[(low >> 6) as usize] >> (low & 63)) & 1 == 0)
                    .collect();
                Container::Array(filtered)
            }
            (Container::Bitmap { words, cardinality }, Container::Array(b)) => {
                let mut words = words.clone();
                let mut cardinality = *cardinality;
                for &low in b {
                    let word = &mut words[(low >> 6) as usize];
                    let mask = 1u64 << (low & 63);
                    if *word & mask != 0 {
                        *word &= !mask;
                        cardinality -= 1;
                    }
                }
                let mut result = Container::Bitmap { words, cardinality };
                result.normalize();
                result
            }
            (
                Container::Bitmap { words: a, .. },
                Container::Bitmap { words: b, .. },
            ) => {
                let mut words = Box::new([0u64; BITMAP_WORDS]);
                let mut cardinality = 0u32;
                for i in 0..BITMAP_WORDS {
                    let w = a[i] & !b[i];
                    words[i] = w;
                    cardinality += w.count_ones();
                }
                let mut result = Container::Bitmap { words, cardinality };
                result.normalize();
                result
            }
        }
    }

    pub fn xor(&self, other: &Container) -> Container {
        match (self, other) {
            (Container::Array(a), Container::Array(b)) => {
                Container::from_sorted_array(merge_xor(a, b))
            }
            _ => {
                let mut result = self.clone();
                result.ensure_bitmap();
                if let Container::Bitmap { words, cardinality } = &mut result {
                    match other {
                        Container::Bitmap { words: other_words, .. } => {
                            let mut count = 0u32;
                            for (w, &o) in words.iter_mut().zip(other_words.iter()) {
                                *w ^= o;
                                count += w.count_ones();
                            }
                            *cardinality = count;
                        }
                        Container::Array(values) => {
                            for &low in values {
                                let word = &mut words[(low >> 6) as usize];
                                let mask = 1u64 << (low & 63);
                                if *word & mask != 0 {
                                    *word &= !mask;
                                    *cardinality -= 1;
                                } else {
                                    *word |= mask;
                                    *cardinality += 1;
                                }
                            }
                        }
                    }
                }
                result.normalize();
                result
            }
        }
    }

    /// Build a container from an already-sorted, deduplicated vector,
    /// choosing the representation by density.
    fn from_sorted_array(values: Vec<u16>) -> Container {
        let mut container = Container::Array(values);
        if let Container::Array(v) = &container {
            if v.len() > ARRAY_TO_BITMAP_THRESHOLD {
                container.convert_to_bitmap();
            }
        }
        container
    }
}

/// Ordered iterator over the low-16-bit values of a container.
pub enum ContainerIter<'a> {
    Array(std::slice::Iter<'a, u16>),
    Bitmap {
        words: &'a [u64; BITMAP_WORDS],
        word_index: usize,
        bits: u64,
    },
}

impl<'a> Iterator for ContainerIter<'a> {
    type Item = u16;

    fn next(&mut self) -> Option<u16> {
        match self {
            ContainerIter::Array(iter) => iter.next().copied(),
            ContainerIter::Bitmap { words, word_index, bits } => loop {
                if *bits != 0 {
                    let bit = bits.trailing_zeros() as u16;
                    *bits &= *bits - 1;
                    return Some(((*word_index as u16) << 6) | bit);
                }
                *word_index += 1;
                if *word_index >= BITMAP_WORDS {
                    return None;
                }
                *bits = words[*word_index];
            },
        }
    }
}

fn merge_union(a: &[u16], b: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] < b[j] {
            out.push(a[i]);
            i += 1;
        } else if b[j] < a[i] {
            out.push(b[j]);
            j += 1;
        } else {
            out.push(a[i]);
            i += 1;
            j += 1;
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

fn merge_intersection(a: &[u16], b: &[u16]) -> Vec<u16> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] < b[j] {
            i += 1;
        } else if b[j] < a[i] {
            j += 1;
        } else {
            out.push(a[i]);
            i += 1;
            j += 1;
        }
    }
    out
}

fn merge_difference(a: &[u16], b: &[u16]) -> Vec<u16> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] < b[j] {
            out.push(a[i]);
            i += 1;
        } else if b[j] < a[i] {
            j += 1;
        } else {
            i += 1;
            j += 1;
        }
    }
    out.extend_from_slice(&a[i..]);
    out
}

fn merge_xor(a: &[u16], b: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] < b[j] {
            out.push(a[i]);
            i += 1;
        } else if b[j] < a[i] {
            out.push(b[j]);
            j += 1;
        } else {
            i += 1;
            j += 1;
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}
