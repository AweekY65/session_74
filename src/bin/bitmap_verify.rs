//! Differential verification of `BitmapIndex` against `std::collections::BTreeSet`.
//!
//! Run with: cargo run --bin bitmap_verify
//! Prints PASS/FAIL per scenario and exits non-zero on any failure.

use bitmap_index::BitmapIndex;
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Deterministic xorshift64* PRNG so runs are reproducible without external
/// dependencies.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    fn below(&mut self, n: u32) -> u32 {
        self.next_u32() % n
    }
}

fn to_vec(index: &BitmapIndex) -> Vec<u32> {
    index.iter().collect()
}

fn set_to_vec(set: &BTreeSet<u32>) -> Vec<u32> {
    set.iter().copied().collect()
}

/// Compare every observable aspect of an index against a reference set.
fn check_equal(index: &BitmapIndex, reference: &BTreeSet<u32>, what: &str) -> Result<(), String> {
    if index.cardinality() != reference.len() as u64 {
        return Err(format!(
            "{what}: cardinality {} != {}",
            index.cardinality(),
            reference.len()
        ));
    }
    let actual = to_vec(index);
    let expected = set_to_vec(reference);
    if actual != expected {
        return Err(format!(
            "{what}: iteration mismatch ({} vs {} elements)",
            actual.len(),
            expected.len()
        ));
    }
    // Spot-check membership on a sample of present and absent values.
    for &v in expected.iter().step_by((expected.len() / 64).max(1)) {
        if !index.contains(v) {
            return Err(format!("{what}: contains({v}) is false but value is present"));
        }
        if index.contains(v.wrapping_add(1)) != reference.contains(&v.wrapping_add(1)) {
            return Err(format!("{what}: contains({}) disagrees", v.wrapping_add(1)));
        }
    }
    Ok(())
}

fn check_set_ops(a: &BitmapIndex, b: &BitmapIndex, sa: &BTreeSet<u32>, sb: &BTreeSet<u32>, what: &str) -> Result<(), String> {
    let expect_union: BTreeSet<u32> = sa.union(sb).copied().collect();
    let expect_inter: BTreeSet<u32> = sa.intersection(sb).copied().collect();
    let expect_diff: BTreeSet<u32> = sa.difference(sb).copied().collect();
    let expect_xor: BTreeSet<u32> = sa.symmetric_difference(sb).copied().collect();

    check_equal(&a.union(b), &expect_union, &format!("{what} union"))?;
    check_equal(&a.intersection(b), &expect_inter, &format!("{what} intersection"))?;
    check_equal(&a.difference(b), &expect_diff, &format!("{what} difference"))?;
    check_equal(&a.xor(b), &expect_xor, &format!("{what} xor"))?;
    // Commutativity spot checks.
    check_equal(&b.union(a), &expect_union, &format!("{what} union (swapped)"))?;
    check_equal(&b.intersection(a), &expect_inter, &format!("{what} intersection (swapped)"))?;
    check_equal(&b.xor(a), &expect_xor, &format!("{what} xor (swapped)"))?;
    Ok(())
}

fn test_sparse_random(rng: &mut Rng) -> Result<(), String> {
    let mut index = BitmapIndex::new();
    let mut reference = BTreeSet::new();
    for _ in 0..20_000 {
        let v = rng.next_u32();
        index.add(v);
        reference.insert(v);
    }
    check_equal(&index, &reference, "sparse random")?;
    // Sparse data must stay in array containers: storage is exactly
    // 2 bytes per value plus 2 bytes of key per container.
    let expected_max = reference.len() * 2 + index.container_count() * 2;
    if index.storage_bytes() > expected_max {
        return Err(format!(
            "sparse random: storage_bytes {} exceeds array-container bound {}",
            index.storage_bytes(),
            expected_max
        ));
    }
    Ok(())
}

fn test_dense_random(rng: &mut Rng) -> Result<(), String> {
    let mut index = BitmapIndex::new();
    let mut reference = BTreeSet::new();
    // Dense clusters: ~50% fill of several 65536-value buckets forces bitmap
    // containers.
    for bucket in [0u32, 1, 7, 1024, 65535] {
        let base = bucket << 16;
        for _ in 0..33_000 {
            let v = base + rng.below(65_536);
            index.add(v);
            reference.insert(v);
        }
    }
    check_equal(&index, &reference, "dense random")?;
    // Dense containers should use the 8 KiB bitmap representation.
    if index.storage_bytes() > 5 * 8192 + 4096 {
        return Err(format!(
            "dense random: storage_bytes {} unexpectedly large",
            index.storage_bytes()
        ));
    }
    Ok(())
}

fn test_random_ops(rng: &mut Rng) -> Result<(), String> {
    let mut index = BitmapIndex::new();
    let mut reference = BTreeSet::new();
    for step in 0..200_000 {
        // Mix dense and sparse value domains to force container conversions.
        let v = match rng.below(3) {
            0 => rng.below(200_000),          // dense low range
            1 => rng.next_u32(),              // full range, sparse
            _ => (rng.below(4) << 16) + rng.below(65_536), // few dense buckets
        };
        match rng.below(3) {
            0 | 1 => {
                if index.add(v) != reference.insert(v) {
                    return Err(format!("random ops step {step}: add({v}) return mismatch"));
                }
            }
            _ => {
                if index.remove(v) != reference.remove(&v) {
                    return Err(format!("random ops step {step}: remove({v}) return mismatch"));
                }
            }
        }
        if step % 10_000 == 0 {
            check_equal(&index, &reference, "random ops")?;
        }
    }
    check_equal(&index, &reference, "random ops final")
}

fn test_empty_and_boundaries() -> Result<(), String> {
    let empty = BitmapIndex::new();
    let empty_set = BTreeSet::new();
    check_equal(&empty, &empty_set, "empty")?;
    check_set_ops(&empty, &empty, &empty_set, &empty_set, "empty/empty")?;

    let mut boundary = BitmapIndex::new();
    let mut reference = BTreeSet::new();
    for v in [0u32, 1, 65535, 65536, 65537, u32::MAX - 1, u32::MAX] {
        boundary.add(v);
        reference.insert(v);
    }
    check_equal(&boundary, &reference, "boundary values")?;
    check_set_ops(&boundary, &empty, &reference, &empty_set, "boundary/empty")?;
    check_set_ops(&empty, &boundary, &empty_set, &reference, "empty/boundary")?;

    // Duplicate adds and removing absent values must be no-ops.
    let mut idx = BitmapIndex::new();
    if !idx.add(42) || idx.add(42) || idx.remove(7) || !idx.remove(42) || idx.remove(42) {
        return Err("add/remove return values incorrect".into());
    }
    if !idx.is_empty() || idx.cardinality() != 0 {
        return Err("index not empty after removing only element".into());
    }
    Ok(())
}

fn test_set_ops_mixed(rng: &mut Rng) -> Result<(), String> {
    // Build indexes of varied densities and cross-check all set operations.
    let mut sparse_a = (BitmapIndex::new(), BTreeSet::new());
    let mut sparse_b = (BitmapIndex::new(), BTreeSet::new());
    let mut dense_a = (BitmapIndex::new(), BTreeSet::new());
    let mut dense_b = (BitmapIndex::new(), BTreeSet::new());

    for _ in 0..8_000 {
        let v = rng.next_u32();
        sparse_a.0.add(v);
        sparse_a.1.insert(v);
        let v = rng.next_u32();
        sparse_b.0.add(v);
        sparse_b.1.insert(v);
    }
    for _ in 0..40_000 {
        // Overlapping dense ranges in the same buckets.
        let v = rng.below(3 * 65_536);
        dense_a.0.add(v);
        dense_a.1.insert(v);
        let v = rng.below(3 * 65_536);
        dense_b.0.add(v);
        dense_b.1.insert(v);
    }

    check_set_ops(&sparse_a.0, &sparse_b.0, &sparse_a.1, &sparse_b.1, "sparse/sparse")?;
    check_set_ops(&dense_a.0, &dense_b.0, &dense_a.1, &dense_b.1, "dense/dense")?;
    check_set_ops(&sparse_a.0, &dense_a.0, &sparse_a.1, &dense_a.1, "sparse/dense")?;
    check_set_ops(&dense_b.0, &sparse_b.0, &dense_b.1, &sparse_b.1, "dense/sparse")?;
    Ok(())
}

fn test_container_conversion() -> Result<(), String> {
    // Fill a single bucket past the array threshold, then drain it back.
    let mut index = BitmapIndex::new();
    let mut reference = BTreeSet::new();
    for v in 0..5_000u32 {
        index.add(v);
        reference.insert(v);
    }
    check_equal(&index, &reference, "conversion grow")?;
    for v in (0..4_900u32).rev() {
        index.remove(v);
        reference.remove(&v);
    }
    check_equal(&index, &reference, "conversion shrink")?;
    if index.storage_bytes() > 1_024 {
        return Err("container did not convert back to array after draining".into());
    }
    Ok(())
}

fn test_serialization(rng: &mut Rng) -> Result<(), String> {
    let mut index = BitmapIndex::new();
    let mut reference = BTreeSet::new();
    for _ in 0..5_000 {
        let v = rng.next_u32();
        index.add(v);
        reference.insert(v);
    }
    for _ in 0..20_000 {
        let v = rng.below(2 * 65_536);
        index.add(v);
        reference.insert(v);
    }

    // Round-trip through bytes.
    let bytes = index.to_bytes();
    let loaded = BitmapIndex::from_bytes(&bytes)
        .map_err(|e| format!("round-trip deserialize failed: {e}"))?;
    check_equal(&loaded, &reference, "serialize round-trip")?;

    // Round-trip through a local file.
    let path = temp_file("bitmap_index_roundtrip.rbmp");
    index
        .save_to_file(&path)
        .map_err(|e| format!("save_to_file failed: {e}"))?;
    let loaded = BitmapIndex::load_from_file(&path)
        .map_err(|e| format!("load_from_file failed: {e}"))?;
    check_equal(&loaded, &reference, "file round-trip")?;
    let _ = std::fs::remove_file(&path);

    // Empty index round-trip.
    let empty = BitmapIndex::new();
    let loaded = BitmapIndex::from_bytes(&empty.to_bytes())
        .map_err(|e| format!("empty round-trip failed: {e}"))?;
    check_equal(&loaded, &BTreeSet::new(), "empty round-trip")?;
    Ok(())
}

fn test_corruption_detection(rng: &mut Rng) -> Result<(), String> {
    let mut index = BitmapIndex::new();
    for _ in 0..10_000 {
        index.add(rng.below(3 * 65_536));
    }
    for _ in 0..1_000 {
        index.add(rng.next_u32());
    }
    let good = index.to_bytes();

    // 1. Bit flips anywhere must be detected (CRC or structural error).
    for trial in 0..64 {
        let mut corrupted = good.clone();
        let pos = (rng.next_u64() as usize) % corrupted.len();
        corrupted[pos] ^= 1 << (trial % 8);
        if BitmapIndex::from_bytes(&corrupted).is_ok() {
            return Err(format!("corruption at byte {pos} not detected"));
        }
    }

    // 2. Truncation must be detected.
    for cut in [1usize, 5, good.len() / 2, good.len() - 1] {
        if BitmapIndex::from_bytes(&good[..cut]).is_ok() {
            return Err(format!("truncation to {cut} bytes not detected"));
        }
    }

    // 3. Bad magic.
    let mut bad = good.clone();
    bad[0] = b'X';
    if BitmapIndex::from_bytes(&bad).is_ok() {
        return Err("bad magic not detected".into());
    }

    // 4. Unsupported version (recompute CRC so only the version is wrong).
    let mut bad = good.clone();
    bad[4..8].copy_from_slice(&99u32.to_le_bytes());
    if BitmapIndex::from_bytes(&bad).is_ok() {
        return Err("unsupported version not detected".into());
    }

    // 5. Trailing garbage after the checksum.
    let mut bad = good.clone();
    bad.extend_from_slice(&[0xAB; 7]);
    if BitmapIndex::from_bytes(&bad).is_ok() {
        return Err("trailing bytes not detected".into());
    }

    // 6. Corrupted file on disk must be rejected by load_from_file.
    let path = temp_file("bitmap_index_corrupt.rbmp");
    let mut corrupted = good.clone();
    let mid = corrupted.len() / 2;
    corrupted[mid] ^= 0xFF;
    std::fs::write(&path, &corrupted).map_err(|e| format!("write corrupt file: {e}"))?;
    if BitmapIndex::load_from_file(&path).is_ok() {
        return Err("corrupted file not detected".into());
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}

fn temp_file(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{}_{}", std::process::id(), name))
}

fn main() {
    let mut rng = Rng::new(0x5EED_5EED_5EED_5EED);
    let tests: Vec<(&str, Box<dyn Fn(&mut Rng) -> Result<(), String>>)> = vec![
        ("sparse random vs BTreeSet", Box::new(|r| test_sparse_random(r))),
        ("dense random vs BTreeSet", Box::new(|r| test_dense_random(r))),
        ("random add/remove/contains ops", Box::new(|r| test_random_ops(r))),
        ("empty sets and boundary integers", Box::new(|_| test_empty_and_boundaries())),
        ("set ops across densities", Box::new(|r| test_set_ops_mixed(r))),
        ("container representation conversion", Box::new(|_| test_container_conversion())),
        ("serialization round-trip", Box::new(|r| test_serialization(r))),
        ("corrupted input detection", Box::new(|r| test_corruption_detection(r))),
    ];

    let mut failures = 0;
    for (name, test) in &tests {
        match test(&mut rng) {
            Ok(()) => println!("PASS  {name}"),
            Err(e) => {
                println!("FAIL  {name}: {e}");
                failures += 1;
            }
        }
    }

    println!();
    if failures == 0 {
        println!("PASS  all {} test groups passed", tests.len());
    } else {
        println!("FAIL  {failures}/{} test groups failed", tests.len());
        std::process::exit(1);
    }
}
