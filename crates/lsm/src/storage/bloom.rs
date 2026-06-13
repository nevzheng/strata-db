//! Bloom filters for membership skipping.

use crate::config::BloomConfig;

/// Fixed seed so a filter hashes identically across runs and processes —
/// required because blooms are persisted in SSTable headers.
const SEED: u128 = 0x6c736d5f626c6f6f6d5f7365656421; // "lsm_bloom_seed!"

/// Approximate set membership over user keys. Built once from a node's
/// keys; a negative `contains` proves absence, so the subtree is skipped.
///
/// Thin wrapper over `fastbloom` to keep the implementation swappable —
/// nothing outside this module names the underlying crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BloomFilter {
    inner: fastbloom::BloomFilter,
}

impl BloomFilter {
    /// Build a filter over `keys`, sized by `config` for `n_keys` entries.
    pub fn build<'a>(
        config: BloomConfig,
        n_keys: usize,
        keys: impl IntoIterator<Item = &'a [u8]>,
    ) -> Self {
        let bits = (config.bits_per_key as usize).saturating_mul(n_keys).max(1);
        let mut inner = fastbloom::BloomFilter::with_num_bits(bits)
            .seed(&SEED)
            .expected_items(n_keys.max(1));
        for key in keys {
            inner.insert(key);
        }
        Self { inner }
    }

    /// `false` proves the key is absent; `true` means "probably present".
    pub fn contains(&self, key: &[u8]) -> bool {
        self.inner.contains(key)
    }

    /// The raw bit blocks — half of the bloom's serializable state.
    pub fn blocks(&self) -> &[u64] {
        self.inner.as_slice()
    }

    /// Number of hashes per item — the other half of the serializable state.
    pub fn num_hashes(&self) -> u32 {
        self.inner.num_hashes()
    }

    /// Rebuild a filter from its raw blocks and hash count, using the same
    /// fixed seed it was built with.
    pub fn from_blocks(blocks: Vec<u64>, num_hashes: u32) -> Self {
        let inner = fastbloom::BloomFilter::from_vec(blocks)
            .seed(&SEED)
            .hashes(num_hashes);
        Self { inner }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_raw_blocks() {
        let keys: Vec<&[u8]> = vec![b"alice", b"bob", b"carol"];
        let bloom = BloomFilter::build(BloomConfig { bits_per_key: 10 }, keys.len(), keys.clone());

        let rebuilt = BloomFilter::from_blocks(bloom.blocks().to_vec(), bloom.num_hashes());

        for k in &keys {
            assert!(rebuilt.contains(k), "{k:?} should be present after rebuild");
        }
        assert!(!rebuilt.contains(b"dave"));
    }
}
