use super::scan::ScanResult;
use crate::node::types::{BlockAndReceipts, EvmBlock};
use rangemap::RangeInclusiveMap;
use reth_network::cache::LruMap;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

#[derive(Debug)]
pub struct LocalBlocksCache {
    cache: LruMap<u64, BlockAndReceipts>,
    cache_size: u32,
    ranges: RangeInclusiveMap<u64, PathBuf>,
}

impl LocalBlocksCache {
    pub fn new(cache_size: u32) -> Self {
        Self { cache: LruMap::new(cache_size), cache_size, ranges: RangeInclusiveMap::new() }
    }

    pub fn load_scan_result(&mut self, scan_result: ScanResult) {
        for blk in scan_result.new_blocks {
            self.insert_block(blk);
        }
        self.load_ranges(scan_result.path, scan_result.new_block_ranges);
    }

    /// Loads a re-read file so that `height` and the blocks after it, up to the cache size, are
    /// the last ones inserted and so stay cached.
    ///
    /// A file can hold more blocks than the cache (hl-node writes a catch-up file after a
    /// stall). Loading it in file order would evict its first blocks again, while the range index
    /// still points at the file, so `height` could never be served from it.
    pub fn load_scan_result_for_height(&mut self, scan_result: ScanResult, height: u64) {
        let window = height..height.saturating_add(self.cache_size as u64);
        let (window_blocks, rest): (Vec<_>, Vec<_>) =
            scan_result.new_blocks.into_iter().partition(|blk| window.contains(&block_number(blk)));
        for blk in rest.into_iter().chain(window_blocks) {
            self.insert_block(blk);
        }
        self.load_ranges(scan_result.path, scan_result.new_block_ranges);
    }

    fn insert_block(&mut self, blk: BlockAndReceipts) {
        self.cache.insert(block_number(&blk), blk);
    }

    fn load_ranges(&mut self, path: PathBuf, ranges: Vec<std::ops::RangeInclusive<u64>>) {
        for range in ranges {
            self.ranges.insert(range, path.clone());
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

fn block_number(blk: &BlockAndReceipts) -> u64 {
    let EvmBlock::Reth115(b) = &blk.block;
    b.header.header.number
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

    fn file_scan(heights: std::ops::Range<u64>) -> ScanResult {
        ScanResult {
            path: "/tmp/hour".into(),
            next_expected_height: heights.end,
            new_blocks: heights.clone().map(|h| test_utils::block(h, 1)).collect(),
            new_block_ranges: std::iter::once(heights.start..=heights.end - 1).collect(),
        }
    }

    #[test]
    fn file_larger_than_cache_loses_its_head_in_file_order() {
        let mut cache = LocalBlocksCache::new(4);
        cache.load_scan_result(file_scan(100..106));

        assert!(cache.get_path_for_height(100).is_some());
        assert!(cache.get_block(100).is_none());
        assert!(cache.get_block(105).is_some());
    }

    #[test]
    fn reload_for_height_keeps_the_requested_window() {
        let mut cache = LocalBlocksCache::new(4);
        cache.load_scan_result(file_scan(100..106));
        assert!(cache.get_block(100).is_none());

        cache.load_scan_result_for_height(file_scan(100..106), 100);
        for height in 100..104 {
            assert!(cache.get_block(height).is_some(), "height {height} should be cached");
        }
        assert!(cache.get_block(104).is_none());
        assert!(cache.get_block(105).is_none());

        // The rest of the file can be reloaded the same way.
        cache.load_scan_result_for_height(file_scan(100..106), 104);
        assert!(cache.get_block(104).is_some());
        assert!(cache.get_block(105).is_some());
    }
}
