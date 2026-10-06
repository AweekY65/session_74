//! A local compressed integer bitmap index for non-negative `u32` integers.
//!
//! Values are bucketed by their high 16 bits. Each bucket ("container") holds
//! the low 16 bits and is stored either as a sorted array (sparse) or a
//! 65536-bit bitmap (dense), chosen by a density threshold. Containers are
//! converted between representations when their cardinality crosses the
//! threshold, without changing the stored set.

mod container;
mod serialize;

use std::collections::BTreeMap;

pub use container::Container;

/// High 16 bits of a value select the container key.
#[inline]
fn key_of(value: u32) -> u16 {
    (value >> 16) as u16
}

/// Low 16 bits of a value are stored inside the container.
#[inline]
fn low_of(value: u32) -> u16 {
    (value & 0xFFFF) as u16
}

/// A compressed bitmap index over `u32` values.
#[derive(Clone, Debug, Default)]
pub struct BitmapIndex {
    containers: BTreeMap<u16, Container>,
}

impl BitmapIndex {
    /// Create an empty index.
    pub fn new() -> Self {
        BitmapIndex {
            containers: BTreeMap::new(),
        }
    }

    /// Build an index from an iterator of values.
    pub fn from_iter<I: IntoIterator<Item = u32>>(values: I) -> Self {
        let mut index = BitmapIndex::new();
        for v in values {
            index.add(v);
        }
        index
    }

    /// Insert `value`. Returns `true` if it was not already present.
    pub fn add(&mut self, value: u32) -> bool {
        let key = key_of(value);
        let container = self
            .containers
            .entry(key)
            .or_insert_with(Container::empty_array);
        container.add(low_of(value))
    }

    /// Remove `value`. Returns `true` if it was present.
    pub fn remove(&mut self, value: u32) -> bool {
        let key = key_of(value);
        let Some(container) = self.containers.get_mut(&key) else {
            return false;
        };
        let removed = container.remove(low_of(value));
        if removed && container.is_empty() {
            self.containers.remove(&key);
        }
        removed
    }

    /// Check membership.
    pub fn contains(&self, value: u32) -> bool {
        self.containers
            .get(&key_of(value))
            .map_or(false, |c| c.contains(low_of(value)))
    }

    /// Number of stored values.
    pub fn cardinality(&self) -> u64 {
        self.containers.values().map(|c| c.cardinality() as u64).sum()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.containers.is_empty()
    }

    /// Number of containers currently in use.
    pub fn container_count(&self) -> usize {
        self.containers.len()
    }

    /// Approximate number of bytes used by the in-memory representation
    /// (container payloads plus per-container bookkeeping).
    pub fn storage_bytes(&self) -> usize {
        self.containers
            .values()
            .map(|c| c.storage_bytes() + std::mem::size_of::<u16>())
            .sum()
    }

    /// Ordered ascending iteration over all values.
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            containers: self.containers.iter(),
            current: None,
        }
    }

    /// Set union: `self | other`.
    pub fn union(&self, other: &BitmapIndex) -> BitmapIndex {
        let mut result = BitmapIndex::new();
        let mut left = self.containers.iter().peekable();
        let mut right = other.containers.iter().peekable();
        loop {
            match (left.peek(), right.peek()) {
                (Some(&(lk, _)), Some(&(rk, _))) => {
                    if lk < rk {
                        result.containers.insert(*lk, left.next().unwrap().1.clone());
                    } else if rk < lk {
                        result.containers.insert(*rk, right.next().unwrap().1.clone());
                    } else {
                        let merged = left.next().unwrap().1.union(right.next().unwrap().1);
                        result.containers.insert(*lk, merged);
                    }
                }
                (Some(&(_, _)), None) => {
                    let (k, c) = left.next().unwrap();
                    result.containers.insert(*k, c.clone());
                }
                (None, Some(&(_, _))) => {
                    let (k, c) = right.next().unwrap();
                    result.containers.insert(*k, c.clone());
                }
                (None, None) => break,
            }
        }
        result
    }

    /// Set intersection: `self & other`.
    pub fn intersection(&self, other: &BitmapIndex) -> BitmapIndex {
        let mut result = BitmapIndex::new();
        // Walk the smaller map and probe the larger one.
        let (small, large) = if self.containers.len() <= other.containers.len() {
            (&self.containers, &other.containers)
        } else {
            (&other.containers, &self.containers)
        };
        for (key, container) in small {
            if let Some(other_container) = large.get(key) {
                let merged = container.intersection(other_container);
                if !merged.is_empty() {
                    result.containers.insert(*key, merged);
                }
            }
        }
        result
    }

    /// Set difference: `self - other`.
    pub fn difference(&self, other: &BitmapIndex) -> BitmapIndex {
        let mut result = BitmapIndex::new();
        for (key, container) in &self.containers {
            let merged = match other.containers.get(key) {
                Some(other_container) => container.difference(other_container),
                None => container.clone(),
            };
            if !merged.is_empty() {
                result.containers.insert(*key, merged);
            }
        }
        result
    }

    /// Symmetric difference: `self ^ other`.
    pub fn xor(&self, other: &BitmapIndex) -> BitmapIndex {
        let mut result = BitmapIndex::new();
        let mut left = self.containers.iter().peekable();
        let mut right = other.containers.iter().peekable();
        loop {
            match (left.peek(), right.peek()) {
                (Some(&(lk, _)), Some(&(rk, _))) => {
                    if lk < rk {
                        result.containers.insert(*lk, left.next().unwrap().1.clone());
                    } else if rk < lk {
                        result.containers.insert(*rk, right.next().unwrap().1.clone());
                    } else {
                        let merged = left.next().unwrap().1.xor(right.next().unwrap().1);
                        if !merged.is_empty() {
                            result.containers.insert(*lk, merged);
                        }
                    }
                }
                (Some(_), None) => {
                    let (k, c) = left.next().unwrap();
                    result.containers.insert(*k, c.clone());
                }
                (None, Some(_)) => {
                    let (k, c) = right.next().unwrap();
                    result.containers.insert(*k, c.clone());
                }
                (None, None) => break,
            }
        }
        result
    }
}

/// Ordered ascending iterator over a [`BitmapIndex`].
pub struct Iter<'a> {
    containers: std::collections::btree_map::Iter<'a, u16, Container>,
    current: Option<(u16, container::ContainerIter<'a>)>,
}

impl<'a> Iterator for Iter<'a> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        loop {
            if let Some((key, iter)) = &mut self.current {
                if let Some(low) = iter.next() {
                    return Some(((*key as u32) << 16) | low as u32);
                }
                self.current = None;
            }
            let (key, container) = self.containers.next()?;
            self.current = Some((*key, container.iter()));
        }
    }
}

impl<'a> IntoIterator for &'a BitmapIndex {
    type Item = u32;
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}
