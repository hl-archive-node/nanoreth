use super::{BlockSource, BlockSourceBoxed};
use crate::node::types::BlockAndReceipts;
use futures::{future::BoxFuture, FutureExt};
use reth_network::cache::LruMap;
use std::sync::{Arc, RwLock};

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

    fn polling_interval(&self) -> std::time::Duration {
        self.block_source.polling_interval()
    }
}
