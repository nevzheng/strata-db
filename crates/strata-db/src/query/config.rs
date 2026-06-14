//! Query execution configuration.
//!
//! Run-time tunables the executor reads while running a query. Today that's just
//! join scratch sizing — the grace hash join's partition buffers and build-side
//! memory ceiling — derived once at startup from the machine's total RAM so the
//! same binary adapts to a laptop and a big server without recompiling.

/// Memory and fan-out limits for the grace hash join. "Partition" and "bucket"
/// are the same thing here — one hash bucket per partition file.
///
/// `Copy` so [`QueryContext`](crate::query::QueryContext) can carry it by value;
/// it's four `usize`s.
#[derive(Clone, Copy, Debug)]
pub struct JoinConfig {
    /// Write buffer per partition file. Full pages spill to disk and the RAM is
    /// reused, so this is the per-partition working set, not its capacity.
    pub partition_memory: usize,
    /// Per-partition on-disk ceiling — the spill backstop.
    pub partition_disk: usize,
    /// Build-side memory ceiling: a partition whose spilled size exceeds this is
    /// repartitioned instead of built in memory.
    pub build_budget: usize,
    /// Bucket fan-out per partitioning pass.
    pub hash_partitions: usize,
}

impl JoinConfig {
    /// Derive limits from the machine's total RAM. Roughly a quarter of memory
    /// is earmarked for query scratch, and the hash-table budget takes a quarter
    /// of that — leaving headroom for the table's live overhead (a `HashMap` of
    /// decoded rows costs several× the spilled bytes it's measured against).
    pub fn from_system_memory(total_bytes: usize) -> Self {
        let scratch = total_bytes / 4;
        Self {
            partition_memory: 4 * 1024 * 1024, // 4 MiB write buffer
            partition_disk: 1 << 32,           // 4 GiB per partition
            build_budget: scratch / 4,         // 1/4 of scratch
            hash_partitions: 32,
        }
    }

    /// Detect total system memory and derive limits, falling back to
    /// [`Default`] (an 8 GiB machine) if detection comes back empty.
    pub fn from_system() -> Self {
        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        match usize::try_from(sys.total_memory()) {
            Ok(total) if total > 0 => Self::from_system_memory(total),
            _ => Self::default(),
        }
    }
}

impl Default for JoinConfig {
    /// Fallback when system-memory detection fails: assume an 8 GiB machine.
    fn default() -> Self {
        Self::from_system_memory(8 * 1024 * 1024 * 1024)
    }
}
