//! Differential verification of BitmapIndex against std BTreeSet.
//! Run: cargo run --bin bitmap_verify
//! Prints PASS/FAIL per scenario; exits non-zero on any failure.

use bitmap_index::BitmapIndex;
use std::collections::BTreeSet;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn check_eq(idx: &BitmapIndex, reference: &BTreeSet<u32>, ctx: &str) -> Result<(), String> {
    if idx.cardinality() != reference.len() as u64 {
        return Err(format!(
            "{ctx}: cardinality {} != {}",
            idx.cardinality(),
            reference.len()
        ));
    }
    let got: Vec<u32> = idx.iter().collect();
    let want: Vec<u32> = reference.iter().copied().collect();
    if got != want {
        return Err(format!("{ctx}: iteration mismatch ({} vs {} values)", got.len(), want.len()));
    }
    Ok(())
}

fn sample_contains(idx: &BitmapIndex, reference: &BTreeSet<u32>, rng: &mut Rng, ctx: &str) -> Result<(), String> {
    for _ in 0..20_000 {
        let v = (rng.next() >> 8) as u32;
        if idx.contains(v) != reference.contains(&v) {
            return Err(format!("{ctx}: contains({v}) mismatch"));
        }
    }
    Ok(())
}

fn build_pair(spec: &[(u32, u32, u64)], rng: &mut Rng) -> (BitmapIndex, BTreeSet<u32>) {
    // spec: (range_start, range_len, count) clusters
    let mut idx = BitmapIndex::new();
    let mut set = BTreeSet::new();
    for &(start, len, count) in spec {
        for _ in 0..count {
            let v = start.wrapping_add(rng.below(len as u64) as u32);
            idx.add(v);
            set.insert(v);
        }
    }
    (idx, set)
}

fn scenario(name: &str, f: impl Fn() -> Result<(), String>) -> bool {
    match f() {
        Ok(()) => {
            println!("PASS  {name}");
            true
        }
        Err(e) => {
            println!("FAIL  {name}: {e}");
            false
        }
    }
}

fn main() {
    let mut ok = true;

    // 1. Sparse data: few values spread across many buckets (array containers).
    ok &= scenario("sparse random", || {
        let mut rng = Rng(0x1111);
        let (idx, set) = build_pair(&[(0, u32::MAX, 30_000)], &mut rng);
        check_eq(&idx, &set, "sparse")?;
        sample_contains(&idx, &set, &mut rng, "sparse")?;
        Ok(())
    });

    // 2. Dense data: ranges that fill buckets past the bitmap threshold.
    ok &= scenario("dense random", || {
        let mut rng = Rng(0x2222);
        let (idx, set) = build_pair(
            &[
                (0, 60_000, 50_000),          // dense in bucket 0
                (1 << 16, 65_536, 60_000),    // dense in bucket 1
                (500 << 16, 65_536, 40_000),  // dense in bucket 500
            ],
            &mut rng,
        );
        check_eq(&idx, &set, "dense")?;
        sample_contains(&idx, &set, &mut rng, "dense")?;
        Ok(())
    });

    // 3. Empty sets.
    ok &= scenario("empty sets", || {
        let a = BitmapIndex::new();
        let b = BitmapIndex::new();
        if a.cardinality() != 0 || !a.is_empty() || a.iter().next().is_some() {
            return Err("empty index not empty".into());
        }
        for (name, r) in [
            ("union", a.union(&b)),
            ("intersection", a.intersection(&b)),
            ("difference", a.difference(&b)),
            ("xor", a.xor(&b)),
        ] {
            if r.cardinality() != 0 {
                return Err(format!("empty {name} not empty"));
            }
        }
        let mut rng = Rng(0x3333);
        let (idx, set) = build_pair(&[(7 << 16, 10_000, 5_000)], &mut rng);
        let empty = BitmapIndex::new();
        check_eq(&idx.union(&empty), &set, "union with empty")?;
        check_eq(&empty.union(&idx), &set, "empty union")?;
        check_eq(&idx.difference(&empty), &set, "difference with empty")?;
        check_eq(&idx.xor(&empty), &set, "xor with empty")?;
        if idx.intersection(&empty).cardinality() != 0 {
            return Err("intersection with empty not empty".into());
        }
        Ok(())
    });

    // 4. Boundary integers: 0, 65535, 65536, u32::MAX, etc.
    ok &= scenario("boundary integers", || {
        let vals = [
            0u32,
            1,
            65535,
            65536,
            65537,
            u32::MAX - 1,
            u32::MAX,
            1 << 31,
            (1 << 31) - 1,
        ];
        let mut idx = BitmapIndex::new();
        let mut set = BTreeSet::new();
        for &v in &vals {
            idx.add(v);
            set.insert(v);
        }
        check_eq(&idx, &set, "boundary")?;
        for &v in &vals {
            if !idx.contains(v) {
                return Err(format!("boundary: missing {v}"));
            }
        }
        // First and last iterated values must be 0 and u32::MAX.
        let all: Vec<u32> = idx.iter().collect();
        if all.first() != Some(&0) || all.last() != Some(&u32::MAX) {
            return Err("boundary: iteration endpoints wrong".into());
        }
        for &v in &vals {
            idx.remove(v);
            set.remove(&v);
        }
        check_eq(&idx, &set, "boundary after remove")?;
        Ok(())
    });

    // 5. Random add/remove workload, continuously diffed against BTreeSet.
    ok &= scenario("random add/remove differential", || {
        let mut rng = Rng(0x5555);
        let mut idx = BitmapIndex::new();
        let mut set = BTreeSet::new();
        for step in 0..200_000u64 {
            // Mix of sparse and dense value generation.
            let v = match rng.below(3) {
                0 => (rng.next() >> 8) as u32,
                1 => (rng.below(200_000)) as u32, // dense low range
                _ => ((rng.below(64) << 16) | rng.below(65_536)) as u32,
            };
            if rng.below(100) < 70 {
                if idx.add(v) != set.insert(v) {
                    return Err(format!("step {step}: add({v}) return mismatch"));
                }
            } else if idx.remove(v) != set.remove(&v) {
                return Err(format!("step {step}: remove({v}) return mismatch"));
            }
            if step % 20_000 == 0 {
                check_eq(&idx, &set, "random workload")?;
            }
        }
        check_eq(&idx, &set, "random workload final")?;
        sample_contains(&idx, &set, &mut rng, "random workload")?;
        Ok(())
    });

    // 6. Set operations vs BTreeSet, across sparse/dense/mixed/disjoint pairs.
    ok &= scenario("set operations differential", || {
        let mut rng = Rng(0x6666);
        let specs: Vec<(&str, Vec<(u32, u32, u64)>, Vec<(u32, u32, u64)>)> = vec![
            ("sparse^sparse", vec![(0, u32::MAX, 20_000)], vec![(0, u32::MAX, 20_000)]),
            ("dense^dense overlap", vec![(0, 50_000, 40_000)], vec![(25_000, 50_000, 40_000)]),
            ("dense^sparse", vec![(3 << 16, 65_536, 50_000)], vec![(0, u32::MAX, 10_000)]),
            ("disjoint", vec![(0, 30_000, 25_000)], vec![(1_000_000, 30_000, 25_000)]),
            ("same bucket", vec![(9 << 16, 65_536, 45_000)], vec![(9 << 16, 65_536, 45_000)]),
            ("empty^dense", vec![], vec![(2 << 16, 65_536, 50_000)]),
        ];
        for (name, sa, sb) in specs {
            let (ia, ra) = build_pair(&sa, &mut rng);
            let (ib, rb) = build_pair(&sb, &mut rng);
            let uni: BTreeSet<u32> = ra.union(&rb).copied().collect();
            let inter: BTreeSet<u32> = ra.intersection(&rb).copied().collect();
            let diff: BTreeSet<u32> = ra.difference(&rb).copied().collect();
            let xor: BTreeSet<u32> = ra.symmetric_difference(&rb).copied().collect();
            check_eq(&ia.union(&ib), &uni, &format!("{name} union"))?;
            check_eq(&ia.intersection(&ib), &inter, &format!("{name} intersection"))?;
            check_eq(&ia.difference(&ib), &diff, &format!("{name} difference"))?;
            check_eq(&ia.xor(&ib), &xor, &format!("{name} xor"))?;
        }
        Ok(())
    });

    // 7. Container conversion must preserve contents across the threshold.
    ok &= scenario("container conversion", || {
        let mut idx = BitmapIndex::new();
        let mut set = BTreeSet::new();
        // Grow one bucket well past ARRAY_LIMIT (array -> bitmap).
        for v in 0..=bitmap_index::ARRAY_LIMIT as u32 + 100 {
            idx.add(v);
            set.insert(v);
        }
        check_eq(&idx, &set, "grow past threshold")?;
        // Shrink back below the threshold (bitmap -> array).
        for v in 0..bitmap_index::ARRAY_LIMIT as u32 {
            idx.remove(v);
            set.remove(&v);
        }
        check_eq(&idx, &set, "shrink below threshold")?;
        // Dense set op results also convert correctly.
        let mut rng = Rng(0x7777);
        let (a, ra) = build_pair(&[(5 << 16, 65_536, 50_000)], &mut rng);
        let (b, rb) = build_pair(&[(5 << 16, 65_536, 50_000)], &mut rng);
        let inter: BTreeSet<u32> = ra.intersection(&rb).copied().collect();
        check_eq(&a.intersection(&b), &inter, "dense intersect")?;
        let xor: BTreeSet<u32> = ra.symmetric_difference(&rb).copied().collect();
        check_eq(&a.xor(&b), &xor, "dense xor")?;
        Ok(())
    });

    // 8. Serialization round-trip via local file.
    ok &= scenario("serialization round-trip", || {
        let mut rng = Rng(0x8888);
        let (idx, set) = build_pair(
            &[(0, u32::MAX, 15_000), (10 << 16, 65_536, 50_000), (0, 100, 100)],
            &mut rng,
        );
        let path = std::env::temp_dir().join("bitmap_verify_roundtrip.bidx");
        idx.save(&path).map_err(|e| e.to_string())?;
        let loaded = BitmapIndex::load(&path).map_err(|e| e.to_string())?;
        std::fs::remove_file(&path).ok();
        check_eq(&loaded, &set, "round-trip")?;
        if loaded.stored_bytes() != idx.stored_bytes() {
            return Err("stored_bytes changed after round-trip".into());
        }
        if idx.serialized_size() != idx.to_bytes().len() as u64 {
            return Err("serialized_size mismatch".into());
        }
        // Empty index round-trip too.
        let empty = BitmapIndex::new();
        let bytes = empty.to_bytes();
        let back = BitmapIndex::from_bytes(&bytes).map_err(|e| e.to_string())?;
        if back.cardinality() != 0 {
            return Err("empty round-trip failed".into());
        }
        Ok(())
    });

    // 9. Corrupted / malformed files must be rejected.
    ok &= scenario("corruption detection", || {
        let mut rng = Rng(0x9999);
        let (idx, _) = build_pair(&[(0, u32::MAX, 5_000), (4 << 16, 65_536, 50_000)], &mut rng);
        let good = idx.to_bytes();

        // Flip a byte in the header, body, and payload: CRC must catch all.
        for pos in [0usize, 5, 11, good.len() / 2, good.len() - 5] {
            let mut bad = good.clone();
            bad[pos] ^= 0xFF;
            if BitmapIndex::from_bytes(&bad).is_ok() {
                return Err(format!("corruption at byte {pos} not detected"));
            }
        }
        // Truncations.
        for cut in [0usize, 3, 8, good.len() / 2, good.len() - 1] {
            if BitmapIndex::from_bytes(&good[..cut]).is_ok() {
                return Err(format!("truncation to {cut} not detected"));
            }
        }
        // Trailing garbage (recompute CRC so only structure check can catch it).
        let mut extra = good.clone();
        extra.extend_from_slice(&[0u8; 3]);
        if BitmapIndex::from_bytes(&extra).is_ok() {
            return Err("trailing garbage not detected".into());
        }
        // Bad version with valid CRC: rebuild bytes manually.
        let mut badver = good.clone();
        badver[4..8].copy_from_slice(&99u32.to_le_bytes());
        let crc = crc32_of(&badver[..badver.len() - 4]);
        let n = badver.len();
        badver[n - 4..].copy_from_slice(&crc.to_le_bytes());
        if BitmapIndex::from_bytes(&badver).is_ok() {
            return Err("bad version not detected".into());
        }
        // Loading a nonexistent path must error, not panic.
        if BitmapIndex::load("/nonexistent_dir_xyz/nope.bidx").is_ok() {
            return Err("missing file not detected".into());
        }
        Ok(())
    });

    // 10. Storage accounting: dense buckets must use far less than 4 bytes/value.
    ok &= scenario("storage bytes accounting", || {
        let mut idx = BitmapIndex::new();
        for v in 0..60_000u32 {
            idx.add(v);
        }
        let bytes = idx.stored_bytes();
        if bytes == 0 || bytes >= idx.cardinality() * 4 {
            return Err(format!(
                "stored_bytes {bytes} not compressed for {} values",
                idx.cardinality()
            ));
        }
        // A dense 65536-bit bucket should cost exactly 8192 bytes of bitmap.
        let mut full = BitmapIndex::new();
        for v in 0..=65535u32 {
            full.add(v);
        }
        if full.stored_bytes() != 8192 {
            return Err(format!("full bucket stored_bytes = {}", full.stored_bytes()));
        }
        Ok(())
    });

    println!();
    if ok {
        println!("PASS  all scenarios");
    } else {
        println!("FAIL  one or more scenarios failed");
        std::process::exit(1);
    }
}

fn crc32_of(data: &[u8]) -> u32 {
    // Recompute CRC the same way the library does, via a tiny local copy,
    // so the verify binary stays self-contained.
    let mut table = [0u32; 256];
    for (i, e) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *e = c;
    }
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = table[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}
