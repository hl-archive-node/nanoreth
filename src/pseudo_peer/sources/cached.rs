use super::{BlockSource, BlockSourceBoxed};
use crate::node::types::BlockAndReceipts;
use futures::{FutureExt, future::BoxFuture};
use reth_network::cache::LruMap;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

/// Block source wrapper that caches blocks in memory
#[derive(Debug, Clone)]
pub struct CachedBlockSource {
    block_source: BlockSourceBoxed,
    cache: Arc<RwLock<LruMap<u64, BlockAndReceipts>>>,
}

impl CachedBlockSource {
    const CACHE_LIMIT: u32 = 100000;

    pub fn new(block_source: BlockSourceBoxed) -> Self {
        Self { block_source, cache: Arc::new(RwLock::new(LruMap::new(Self::CACHE_LIMIT))) }
    }
}

impl BlockSource for CachedBlockSource {
    fn collect_block(&self, height: u64) -> BoxFuture<'static, eyre::Result<BlockAndReceipts>> {
        let block_source = self.block_source.clone();
        let cache = self.cache.clone();
        async move {
            if let Some(block) = cache.write().unwrap().get(&height) {
                return Ok(block.clone());
            }
            let block = block_source.collect_block(height).await?;
            cache.write().unwrap().insert(height, block.clone());
            Ok(block)
        }
        .boxed()
    }

    fn find_latest_block_number(&self) -> BoxFuture<'static, Option<u64>> {
        self.block_source.find_latest_block_number()
    }

    fn recommended_chunk_size(&self) -> u64 {
        self.block_source.recommended_chunk_size()
    }

    fn collect_blocks(
        &self,
        heights: Vec<u64>,
    ) -> BoxFuture<'static, eyre::Result<Vec<BlockAndReceipts>>> {
        let block_source = self.block_source.clone();
        let cache = self.cache.clone();
        async move {
            // Split into cached and uncached
            let mut cached: HashMap<u64, BlockAndReceipts> = HashMap::new();
            let mut uncached_heights = Vec::new();
            {
                let mut c = cache.write().unwrap();
                for &h in &heights {
                    if let Some(block) = c.get(&h) {
                        cached.insert(h, block.clone());
                    } else {
                        uncached_heights.push(h);
                    }
                }
            }

            // Batch fetch uncached blocks from inner source
            if !uncached_heights.is_empty() {
                let fetched = block_source.collect_blocks(uncached_heights).await?;
                let mut c = cache.write().unwrap();
                for block in fetched {
                    let h = block.number();
                    c.insert(h, block.clone());
                    cached.insert(h, block);
                }
            }

            // Return in original order
            heights
                .iter()
                .map(|h| cached.remove(h).ok_or_else(|| eyre::eyre!("Block {h} not found")))
                .collect()
        }
        .boxed()
    }

    fn polling_interval(&self) -> std::time::Duration {
        self.block_source.polling_interval()
    }
}
