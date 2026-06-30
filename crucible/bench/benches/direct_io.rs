//! Benchmarks comparing buffered vs direct I/O paths in [`DirectFile`].
//!
//! These measure raw block-level throughput — they isolate the I/O layer
//! from the page cache and query engine.  The first run always prints
//! whether direct I/O actually engaged on this platform/filesystem, so
//! you know whether the "direct" numbers reflect O_DIRECT / F_NOCACHE or
//! the fallback path.
//!
//! ## Results (2026-06-26, macOS 15, Apple M4, APFS)
//!
//! F_NOCACHE engaged (is_direct() = true).  Direct I/O is slower on raw
//! throughput because the OS page cache provides read-ahead and write-back
//! coalescing that direct I/O forfeits.  The direct path measures real
//! SSD cost; the buffered path measures OS-cache-assisted cost.
//!
//! Absolute values are hardware-dependent.  The structural result — that
//! direct I/O is slower on microbenchmarks because the OS cache absorbs
//! the I/O — holds on any hardware with free RAM.  The db_workload sweep
//! shows where it actually wins.
//!
//! | Benchmark | Buffered | Direct | Δ |
//! |-----------|----------|--------|---|
//! | Random reads (8 KiB) | 10.1 MiB/s | 6.9 MiB/s | −32% |
//! | Random writes (8 KiB + fsync) | 1.7 MiB/s | 0.6 MiB/s | −65% |
//! | Sequential scan (4 MiB) | 8.5 GiB/s | 6.0 GiB/s | −29% |
//!
//! ## Interpreting the numbers
//!
//! On small working sets (fits in RAM), the **buffered** path will appear
//! faster because it reads from the OS page cache (RAM), not from disk.
//! The **direct** path bypasses the OS page cache, so every read hits
//! the storage device — it measures real I/O cost.
//!
//! The win for direct I/O is *not* raw throughput on a hot cache.  It is:
//! - **Memory efficiency**: no double buffering (DB pool + OS cache).
//!   With buffered I/O the OS keeps a copy of every page the DB reads,
//!   roughly doubling RAM usage.  Direct I/O eliminates the OS copy,
//!   freeing that RAM for a larger buffer pool or query execution.
//! - **Predictable latency**: the DB's LRU-K policy controls eviction,
//!   not the kernel's LRU, which would fight it.
//! - **Working-set isolation**: a sequential scan cannot evict hot pages
//!   from the OS cache because there *is* no OS cache copy.
//!
//! See the db_workload benchmark for the inversion-point sweep that
//! shows where direct I/O becomes the right choice.
//!
//! Run with:
//!   cargo bench -p bench
//!
//! Or to compare buffered vs direct side-by-side:
//!   cargo bench -p bench -- direct

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use filesystem::block::{Block, DirectFile};
use std::io;
use tempfile::tempdir;

const BLOCK: usize = 8192;

/// Create a file pre-populated with `num_blocks` blocks of deterministic
/// data.  Always uses buffered I/O so the setup is fast and identical for
/// both benchmark variants — the file on disk is the same either way.
fn populate(path: &std::path::Path, num_blocks: usize) -> io::Result<()> {
    let f = DirectFile::open_buffered(path)?;
    let block = patterned_block(0xFF);
    for i in 0..num_blocks {
        f.write_all_at(&block, (i * BLOCK) as u64)?;
    }
    f.sync_all()?;
    Ok(())
}

/// A deterministic 8 KiB block whose contents depend on `seed`.
fn patterned_block(seed: u8) -> Block {
    let mut block = Block::zeroed();
    for (i, byte) in block.iter_mut().enumerate() {
        *byte = seed.wrapping_add(i as u8).wrapping_mul(37);
    }
    block
}

// ---------------------------------------------------------------------------
// Benchmark groups
// ---------------------------------------------------------------------------

fn bench_random_reads(c: &mut Criterion) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("randread.db");
    let num_blocks = 1024; // 8 MiB working set
    populate(&path, num_blocks).unwrap();

    // Pre-compute a random-ish access order so the benchmark only
    // measures I/O, not RNG.
    let order: Vec<u64> = (0..num_blocks)
        .map(|i| {
            let idx = (i.wrapping_mul(2654435761usize).wrapping_add(13)) % num_blocks;
            (idx * BLOCK) as u64
        })
        .collect();

    let mut group = c.benchmark_group("random-read");
    group.throughput(Throughput::Bytes(BLOCK as u64));
    group.sample_size(50);

    // Pre-allocate a Block once (matches how PageCache frames work —
    // they are pre-allocated and aligned, not allocated per I/O).
    let mut buf = Block::zeroed();

    // --- Buffered ---
    {
        let f = DirectFile::open_buffered(&path).unwrap();
        group.bench_function(BenchmarkId::new("read", "buffered"), |b| {
            b.iter(|| {
                for &off in &order {
                    f.read_exact_at(black_box(&mut buf), black_box(off))
                        .unwrap();
                }
            });
        });
    }

    // --- Direct ---
    {
        let f = DirectFile::open(&path).unwrap();
        let label = if f.is_direct() {
            "direct(O_DIRECT)"
        } else {
            "direct(fallback)"
        };
        group.bench_function(BenchmarkId::new("read", label), |b| {
            b.iter(|| {
                for &off in &order {
                    f.read_exact_at(black_box(&mut buf), black_box(off))
                        .unwrap();
                }
            });
        });
    }

    group.finish();
}

fn bench_random_writes(c: &mut Criterion) {
    let num_blocks = 256; // 2 MiB
    let block = patterned_block(0xAA);

    let mut group = c.benchmark_group("random-write");
    group.throughput(Throughput::Bytes(BLOCK as u64));
    group.sample_size(30);

    // --- Buffered ---
    {
        let dir = tempdir().unwrap();
        let path = dir.path().join("randwrite_buf.db");
        let f = DirectFile::open_buffered(&path).unwrap();
        group.bench_function(BenchmarkId::new("write", "buffered"), |b| {
            b.iter(|| {
                for i in 0..num_blocks {
                    f.write_all_at(black_box(&block), black_box((i * BLOCK) as u64))
                        .unwrap();
                }
                f.sync_all().unwrap();
            });
        });
    }

    // --- Direct ---
    {
        let dir = tempdir().unwrap();
        let path = dir.path().join("randwrite_dir.db");
        let f = DirectFile::open(&path).unwrap();
        let label = if f.is_direct() {
            "direct(O_DIRECT)"
        } else {
            "direct(fallback)"
        };
        group.bench_function(BenchmarkId::new("write", label), |b| {
            b.iter(|| {
                for i in 0..num_blocks {
                    f.write_all_at(black_box(&block), black_box((i * BLOCK) as u64))
                        .unwrap();
                }
                f.sync_all().unwrap();
            });
        });
    }

    group.finish();
}

fn bench_sequential_scan(c: &mut Criterion) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("seqscan.db");
    let num_blocks = 512; // 4 MiB
    populate(&path, num_blocks).unwrap();

    let mut group = c.benchmark_group("sequential-scan");
    group.throughput(Throughput::Bytes((num_blocks * BLOCK) as u64));
    group.sample_size(30);

    // Pre-allocate a Block once.
    let mut buf = Block::zeroed();

    // --- Buffered ---
    {
        let f = DirectFile::open_buffered(&path).unwrap();
        group.bench_function(BenchmarkId::new("scan", "buffered"), |b| {
            b.iter(|| {
                for i in 0..num_blocks {
                    f.read_exact_at(black_box(&mut buf), black_box((i * BLOCK) as u64))
                        .unwrap();
                }
            });
        });
    }

    // --- Direct ---
    {
        let f = DirectFile::open(&path).unwrap();
        let label = if f.is_direct() {
            "direct(O_DIRECT)"
        } else {
            "direct(fallback)"
        };
        group.bench_function(BenchmarkId::new("scan", label), |b| {
            b.iter(|| {
                for i in 0..num_blocks {
                    f.read_exact_at(black_box(&mut buf), black_box((i * BLOCK) as u64))
                        .unwrap();
                }
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_random_reads,
    bench_random_writes,
    bench_sequential_scan
);
criterion_main!(benches);
