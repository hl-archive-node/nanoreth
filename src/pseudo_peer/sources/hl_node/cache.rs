use super::scan::ScanResult;
use crate::node::types::{BlockAndReceipts, EvmBlock};
use rangemap::RangeInclusiveMap;
use reth_network::cache::LruMap;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

#[derive(Debug)]
pub struct LocalBlocksCache {
    cache: LruMap<u64, BlockAndReceipts>,
    ranges: RangeInclusiveMap<u64, PathBuf>,
}

impl LocalBlocksCache {
    pub fn new(cache_size: u32) -> Self {
        Self { cache: LruMap::new(cache_size), ranges: RangeInclusiveMap::new() }
    }

    pub fn load_scan_result(&mut self, scan_result: ScanResult) {
        for blk in scan_result.new_blocks {
            let EvmBlock::Reth115(b) = &blk.block;
            self.cache.insert(b.header.header.number, blk);
        }
        for range in scan_result.new_block_ranges {
            self.ranges.insert(range, scan_result.path.clone());
        }
    }

    pub fn get_block(&mut self, height: u64) -> Option<BlockAndReceipts> {
        self.cache.get(&height).cloned()
    }

    pub fn remove_block(&mut self, height: u64) {
        self.cache.remove(&height);
    }

    pub fn get_path_for_height(&self, height: u64) -> Option<PathBuf> {
        self.ranges.get(&height).cloned()
    }

    pub fn log_range_summary(&self, root: &Path) {
        if self.ranges.is_empty() {
            warn!("No ranges found in {:?}", root);
        } else {
            let (min, max) =
                (self.ranges.first_range_value().unwrap(), self.ranges.last_range_value().unwrap());
            info!(
                "Populated {} ranges (min: {}, max: {})",
                self.ranges.len(),
                min.0.start(),
                max.0.end()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::scan::ScanResult;
    use super::*;
    use crate::pseudo_peer::sources::test_utils;

    #[test]
    fn invalidation_allows_replacement_at_same_height() {
        let old = test_utils::block(42, 1);
        let new = test_utils::block(42, 2);
        let result = |block| ScanResult {
            path: "/tmp/hour".into(),
            next_expected_height: 43,
            new_blocks: vec![block],
            new_block_ranges: std::iter::once(42..=42).collect(),
        };
        let mut cache = LocalBlocksCache::new(4);

        cache.load_scan_result(result(old.clone()));
        cache.remove_block(42);
        cache.load_scan_result(result(new.clone()));

        let refreshed = cache.get_block(42).unwrap();
        assert_eq!(refreshed.hash(), new.hash());
        assert_eq!(refreshed.highest_precompile_address, new.highest_precompile_address);
        assert_ne!(refreshed.highest_precompile_address, old.highest_precompile_address);
    }
}
