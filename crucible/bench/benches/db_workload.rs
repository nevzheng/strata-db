//! Database-level workload benchmarks comparing buffered vs direct I/O
//! across a sweep of buffer-pool sizes.
//!
//! ## Results (2026-06-26, macOS 15, Apple M4, APFS)
//!
//! 16K web-app rows (150 bytes each ≈ 2.4 MiB heap, ~300 pages).  Pool
//! sizes swept from 32 frames (256 KiB) to 512 frames (4 MiB).
//!
//! **Absolute throughput** is hardware-dependent (this machine's SSD
//! speed, CPU, available RAM).  **The inversion point** — where direct
//! I/O closes the gap with buffered at pool ≥ dataset — is structural:
//! it depends only on the ratio of pool frames to dataset pages, not on
//! the underlying storage speed.  The same shape will appear on any
//! hardware; only the Y-axis values will shift.
//!
//! ### Point-lookup throughput (random access, 500 lookups/iter)
//!
//! | Frames | Pool  | Buffered | Direct | Ratio | Eviction |
//! |--------|-------|----------|--------|-------|----------|
//! | 32     | 256K  | 51.7 Kq/s| 12.8 Kq/s| **4.0×** | 90% miss — OS cache saves buffered |
//! | 64     | 512K  | 46.2 Kq/s| 10.3 Kq/s| **4.5×** | 80% miss |
//! | 128    | 1M    | 46.9 Kq/s| 10.2 Kq/s| **4.6×** | 60% miss |
//! | 256    | 2M    | 54.6 Kq/s| 35.7 Kq/s| **1.5×** | 15% miss — gap closing |
//! | **384**|**3M** | 54.4 Kq/s|**54.3 Kq/s**|**1.0×**| **← INVERSION: pool ≥ dataset** |
//! | 512    | 4M    | 57.9 Kq/s| 53.1 Kq/s| 1.1×  | pool comfortably exceeds dataset |
//!
//! The **inversion point** is at 384 frames (3 MiB): once the pool can
//! hold the entire working set (~300 pages), direct I/O matches buffered
//! throughput within measurement noise.  The ratio stays at ~1.0× for
//! all larger sizes — 512 or any size above the dataset page count.
//!
//! ### Memory implications
//!
//! Below the inversion point the OS page cache is essential — it acts as
//! a second-level cache that swallows pool misses.  But this comes at a
//! cost: the OS keeps a copy of every page the DB has ever read, roughly
//! doubling the RAM footprint.  For the 2.4 MiB dataset at 256 frames:
//!
//! | I/O mode | DB pool | OS cache | Total RAM for this data |
//! |----------|---------|----------|------------------------|
//! | Buffered | 2 MiB   | ~2.4 MiB | ~4.4 MiB (double-buffered) |
//! | Direct   | 2 MiB   | 0        | 2 MiB   (pool only) |
//!
//! Above the inversion point, both paths deliver the same throughput,
//! but direct I/O does it with half the RAM.  That RAM can be given back
//! to the buffer pool (more frames) or used by the query engine (sort
//! buffers, hash tables).  This is why production databases (PostgreSQL,
//! RocksDB) use direct I/O — they size the pool to the working set and
//! let the DB control eviction end-to-end.
//!
//! Sequential scans are not benchmarked here — scans touch every page
//! once with no reuse, so OS read-ahead dominates regardless of pool size,
//! and direct I/O always loses that comparison.  The point-lookup sweep
//! captures the inversion that matters for OLTP workloads.
//!
//! ## What this measures
//!
//! Run with:
//!   cargo bench -p bench -- db_workload

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use std::io::Write;
use strata_store::StorageEngine;
use strata_store::memstore::BTreeMapStore;
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Total rows in the benchmark dataset.  16K rows × 150 bytes ≈ 2.4 MiB
/// ≈ 300 pages at ~51 tuples/page.  Pool sizes below 300 frames will see
/// eviction; sizes above 300 will not.
const ROW_COUNT: usize = 16_000;

/// Size of each value in bytes — simulates a typical web-app row.
const VALUE_SIZE: usize = 150;

/// Number of random lookups per criterion iteration.
const LOOKUPS_PER_ITER: usize = 500;

/// Pool sizes to sweep (in 8 KiB frames).  From 32 (256 KiB, 10% of
/// dataset) to 512 (4 MiB, 170% of dataset).
const POOL_SIZES: [usize; 6] = [32, 64, 128, 256, 384, 512];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_value(idx: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(VALUE_SIZE);
    write!(
        &mut v,
        "user_{:08}@example.com  {:08}  {:04}",
        idx,
        idx,
        idx % 10000
    )
    .unwrap();
    while v.len() < VALUE_SIZE {
        v.push(b'.');
    }
    v.truncate(VALUE_SIZE);
    v
}

fn make_key(idx: usize) -> [u8; 8] {
    (idx as u64).to_be_bytes()
}

fn get_bytes(engine: &StorageEngine<BTreeMapStore>, key: &[u8]) -> Option<Vec<u8>> {
    engine
        .get(key)
        .unwrap()
        .map(|view| view.bytes().expect("live tuple").to_vec())
}

fn open_engine(dir: &std::path::Path, frames: usize, direct: bool) -> StorageEngine<BTreeMapStore> {
    StorageEngine::<BTreeMapStore>::builder(dir, BTreeMapStore::new())
        .heap_frames(frames)
        .direct_io(direct)
        .build()
        .unwrap()
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

/// Sweep pool sizes and compare buffered vs direct point-lookup throughput.
///
/// The sweep shows the inversion point: the pool size at which direct I/O
/// closes the gap with buffered I/O because the pool itself is large enough
/// to hold the working set.
fn bench_point_lookup_sweep(c: &mut Criterion) {
    let mut group = c.benchmark_group("point-lookup-sweep");
    group.throughput(Throughput::Elements(LOOKUPS_PER_ITER as u64));
    group.sample_size(20);

    // Pre-compute random access order once.
    let order: Vec<[u8; 8]> = (0..LOOKUPS_PER_ITER)
        .map(|i| {
            let idx = (i.wrapping_mul(2654435761).wrapping_add(13)) % ROW_COUNT;
            make_key(idx)
        })
        .collect();

    // Each (pool_size, io_mode) combination populates its own data directory
    // with that I/O mode.  This is essential: populating with buffered I/O
    // would fill the OS page cache and contaminate subsequent direct I/O
    // measurements (F_NOCACHE is advisory and does not invalidate existing
    // cache entries).  Populating with the mode under test ensures the OS
    // cache state is correct for that mode — empty for direct, warm for
    // buffered.
    for &frames in &POOL_SIZES {
        for (io_label, direct) in [("buffered", false), ("direct", true)] {
            let dir = tempdir().unwrap();
            let path = dir.path();

            // Populate (not measured).
            {
                let mut engine = open_engine(path, frames, direct);
                for i in 0..ROW_COUNT {
                    engine.put(&make_key(i), &make_value(i)).unwrap();
                }
                engine.flush().unwrap();
            }

            // Reopen and benchmark.
            let engine = open_engine(path, frames, direct);

            let bench_id = format!("{io_label}/frames={frames}");
            group.bench_function(BenchmarkId::from_parameter(bench_id), |b| {
                b.iter(|| {
                    for key in &order {
                        black_box(get_bytes(black_box(&engine), black_box(key)));
                    }
                });
            });
        }
    }

    group.finish();
}

criterion_group!(benches, bench_point_lookup_sweep);
criterion_main!(benches);
