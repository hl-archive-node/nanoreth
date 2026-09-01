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
