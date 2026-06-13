//! Configuration, mirroring the data hierarchy: an [`LsmConfig`] is N
//! [`LevelConfig`]s. Each level configures its runs ([`RunConfig`]) and its
//! tables ([`TableConfig`]) directly; the runs and tables created there
//! follow those settings.
//!
//! Build a baseline with [`LsmConfig::leveled`], then tune any level in place.

const KIB: usize = 1024;
const MIB: usize = 1024 * 1024;

/// Bloom sizing as bits per key. The optimal hash count and the resulting
/// false-positive rate both derive from this one knob.
#[derive(Debug, Clone, Copy)]
pub struct BloomConfig {
    pub bits_per_key: u32,
}

impl Default for BloomConfig {
    fn default() -> Self {
        Self { bits_per_key: 10 } // ≈ 1% false positives
    }
}

impl BloomConfig {
    /// Optimal hash count for this density: `k = ln2 · bits_per_key`.
    pub fn hashes(&self) -> u32 {
        ((self.bits_per_key as f64) * std::f64::consts::LN_2)
            .round()
            .max(1.0) as u32
    }

    /// Derived false-positive rate at the optimal `k` (≈ `0.6185 ^ bits_per_key`).
    pub fn fp_rate(&self) -> f64 {
        0.6185_f64.powi(self.bits_per_key as i32)
    }
}

/// How a table is divided into pages on disk and in the cache.
#[derive(Debug, Clone, Copy)]
pub struct PageConfig {
    pub page_size_bytes: usize,
}

impl Default for PageConfig {
    fn default() -> Self {
        Self {
            page_size_bytes: 4 * KIB,
        }
    }
}

/// Per-run policy.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// Size budget for a run before compaction moves it down a level.
    pub max_size_bytes: usize,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            max_size_bytes: 64 * MIB,
        }
    }
}

/// Per-table (SSTable file) policy.
#[derive(Debug, Clone)]
pub struct TableConfig {
    /// Start a new file once the current one exceeds this many bytes.
    pub max_file_size_bytes: usize,
    pub bloom: BloomConfig,
    pub page: PageConfig,
}

impl Default for TableConfig {
    fn default() -> Self {
        Self {
            max_file_size_bytes: 64 * MIB,
            bloom: BloomConfig::default(),
            page: PageConfig::default(),
        }
    }
}

/// Per-level policy: how many runs the level holds, plus the run and table
/// settings everything at this level is built with.
#[derive(Debug, Clone)]
pub struct LevelConfig {
    /// Run count that triggers compaction (high at L0, 1 under leveling).
    pub max_runs: usize,
    pub run: RunConfig,
    pub table: TableConfig,
}

impl Default for LevelConfig {
    fn default() -> Self {
        Self {
            max_runs: 1,
            run: RunConfig::default(),
            table: TableConfig::default(),
        }
    }
}

/// Eviction policy for the page cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CachePolicy {
    /// Evict the least-recently-used page when over budget.
    #[default]
    Lru,
}

/// Size budget for the page cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeConfig {
    /// Cap total cached page bytes.
    Bytes(usize),
    /// Cap the number of cached pages.
    Pages(usize),
    /// Unbounded — cache everything (handy in tests).
    Unbounded,
}

impl Default for SizeConfig {
    fn default() -> Self {
        SizeConfig::Bytes(64 * MIB)
    }
}

/// Page cache configuration: how large it may grow and how it evicts.
#[derive(Debug, Clone, Copy, Default)]
pub struct PageCacheConfig {
    pub size: SizeConfig,
    pub policy: CachePolicy,
}

/// Whole-tree configuration: one [`LevelConfig`] per level, plus the
/// tree-wide page cache.
#[derive(Debug, Clone)]
pub struct LsmConfig {
    pub levels: Vec<LevelConfig>,
    pub page_cache: PageCacheConfig,
}

impl Default for LsmConfig {
    fn default() -> Self {
        Self::leveled(7)
    }
}

impl LsmConfig {
    /// A standard leveled tree of `num_levels` levels: L0 accepts many runs
    /// (one per memtable flush), L1+ keep a single run with the run-size
    /// budget growing by 2× per level. Each level starts from
    /// [`LevelConfig::default`]; tune `levels` afterward as needed.
    pub fn leveled(num_levels: usize) -> Self {
        let levels = (0..num_levels)
            .map(|i| {
                let mut level = LevelConfig::default();
                if i == 0 {
                    level.max_runs = 64;
                } else {
                    level.run.max_size_bytes = 64 * MIB * (1 << i);
                }
                level
            })
            .collect();
        Self {
            levels,
            page_cache: PageCacheConfig::default(),
        }
    }

    pub fn num_levels(&self) -> usize {
        self.levels.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leveled_shapes_l0_and_deeper_levels() {
        let cfg = LsmConfig::leveled(3);
        assert_eq!(cfg.num_levels(), 3);
        // L0 is tiered (many runs); deeper levels keep a single run.
        assert_eq!(cfg.levels[0].max_runs, 64);
        assert_eq!(cfg.levels[1].max_runs, 1);
        // Run-size budget doubles per level below L0.
        assert!(cfg.levels[2].run.max_size_bytes > cfg.levels[1].run.max_size_bytes);
    }

    #[test]
    fn levels_are_tunable_in_place() {
        let mut cfg = LsmConfig::leveled(2);
        cfg.levels[0].table.page.page_size_bytes = 8 * KIB;
        cfg.levels[1].table.bloom.bits_per_key = 16;
        assert_eq!(cfg.levels[0].table.page.page_size_bytes, 8 * KIB);
        assert_eq!(cfg.levels[1].table.bloom.bits_per_key, 16);
    }
}
