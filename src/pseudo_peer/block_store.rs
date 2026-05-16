use super::{patch::testnet_gap_blocks, sources::BlockSourceBoxed, utils::LruBiMap};
use crate::node::types::BlockAndReceipts;
use alloy_primitives::B256;
use futures::future::BoxFuture;
use parking_lot::RwLock;
use reth_network::cache::LruMap;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tracing::{info, warn};

/// Function that resolves a block hash to its number via the node's database.
/// Returns `None` if the hash is not yet in the database (e.g. headers not synced yet).
/// Note: This queries the HeaderNumbers table which covers both database and static files.
pub type DbBlockNumberFn = Arc<dyn Fn(B256) -> Option<u64> + Send + Sync>;

const BLOCK_CACHE_LIMIT: u32 = 100_000;
const HASH_INDEX_LIMIT: u32 = 1_000_000;
const LATEST_BLOCK_RETRY_BACKOFF_AFTER: Duration = Duration::from_secs(2);
const LATEST_BLOCK_RETRY_MAX_INTERVAL: Duration = Duration::from_secs(30);

/// Unified block store that combines block content caching, hash↔number indexing,
/// and database fallback into a single abstraction.
///
/// Every block that passes through the store has its hash (and parent hash)
/// automatically indexed, eliminating scattered cache population.
pub struct BlockStore {
    /// Block content cache: number → block
    blocks: RwLock<LruMap<u64, BlockAndReceipts>>,
    /// Hash index: hash ↔ number (bidirectional)
    hash_index: RwLock<LruBiMap<B256, u64>>,
    /// DB fallback for hash→number (HeaderNumbers table)
    db_block_number: Option<DbBlockNumberFn>,
    /// Underlying fetch source (S3, RPC, etc.)
    source: BlockSourceBoxed,
    /// Hardcoded testnet gap blocks missing from upstream sources
    gap_blocks: HashMap<u64, BlockAndReceipts>,
}

impl std::fmt::Debug for BlockStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockStore").finish_non_exhaustive()
    }
}

impl BlockStore {
    pub fn new(
        source: BlockSourceBoxed,
        db_block_number: Option<DbBlockNumberFn>,
        chain_id: u64,
    ) -> Self {
        let gap_blocks = testnet_gap_blocks(chain_id);
        if !gap_blocks.is_empty() {
            let mut nums: Vec<_> = gap_blocks.keys().copied().collect();
            nums.sort();
            info!(
                "Loaded {} hardcoded gap block(s): {}..={}",
                gap_blocks.len(),
                nums.first().unwrap(),
                nums.last().unwrap(),
            );
        }
        Self {
            blocks: RwLock::new(LruMap::new(BLOCK_CACHE_LIMIT)),
            hash_index: RwLock::new(LruBiMap::new(HASH_INDEX_LIMIT)),
            db_block_number,
            source,
            gap_blocks,
        }
    }

    /// Index a block's hash and parent hash in the hash↔number map.
    pub fn index_block(&self, block: &BlockAndReceipts) {
        let mut idx = self.hash_index.write();
        Self::index_block_inner(&mut idx, block);
    }

    /// Index a block into an already-held write guard. Avoids repeated lock acquisition.
    fn index_block_inner(idx: &mut LruBiMap<B256, u64>, block: &BlockAndReceipts) {
        let number = block.number();
        idx.insert(block.hash(), number);
        if number > 0 {
            idx.insert(block.parent_hash(), number - 1);
        }
    }

    /// Fetch a single block by number. Auto-indexes and caches.
    pub async fn get_by_number(&self, n: u64) -> eyre::Result<BlockAndReceipts> {
        if let Some(block) = self.blocks.write().get(&n) {
            return Ok(block.clone());
        }
        if let Some(block) = self.gap_blocks.get(&n) {
            let block = block.clone();
            self.blocks.write().insert(n, block.clone());
            self.index_block(&block);
            return Ok(block);
        }
        let block = self.source.collect_block(n).await?;
        self.blocks.write().insert(n, block.clone());
        self.index_block(&block);
        Ok(block)
    }

    /// Fetch multiple blocks by number. Auto-indexes and caches.
    pub async fn get_by_numbers(
        &self,
        heights: Vec<u64>,
    ) -> eyre::Result<Vec<BlockAndReceipts>> {
        let mut cached: HashMap<u64, BlockAndReceipts> = HashMap::new();
        let mut uncached_heights = Vec::new();
        {
            let mut c = self.blocks.write();
            for &h in &heights {
                if let Some(block) = c.get(&h) {
                    cached.insert(h, block.clone());
                } else if let Some(block) = self.gap_blocks.get(&h) {
                    c.insert(h, block.clone());
                    cached.insert(h, block.clone());
                } else {
                    uncached_heights.push(h);
                }
            }
        }

        if !uncached_heights.is_empty() {
            let fetched = self.source.collect_blocks(uncached_heights).await?;
            let mut c = self.blocks.write();
            for block in fetched {
                let h = block.number();
                c.insert(h, block.clone());
                cached.insert(h, block);
            }
        }

        // Batch-index all blocks under a single write lock
        {
            let mut idx = self.hash_index.write();
            for block in cached.values() {
                Self::index_block_inner(&mut idx, block);
            }
        }

        heights
            .iter()
            .map(|h| cached.remove(h).ok_or_else(|| eyre::eyre!("Block {h} not found")))
            .collect()
    }

    /// Resolve a block hash to a block number.
    /// Checks the in-memory index first, then falls back to the database.
    pub fn hash_to_number(&self, hash: B256) -> eyre::Result<u64> {
        // Fast path: in-memory index
        if let Some(n) = self.hash_index.read().get_by_left(&hash).copied() {
            return Ok(n);
        }

        // Fallback: database lookup (MDBX is mmap'd, no need to re-cache)
        if let Some(ref db_fn) = self.db_block_number
            && let Some(n) = db_fn(hash) {
                return Ok(n);
            }

        Err(eyre::eyre!("Hash not found in index or database: {hash:?}"))
    }

    // --- Delegated block source methods ---

    pub fn find_latest_block_number(&self) -> BoxFuture<'static, eyre::Result<Option<u64>>> {
        self.source.find_latest_block_number()
    }

    pub async fn wait_for_latest_block_number(&self) -> u64 {
        let polling_interval = self.polling_interval();
        let mut retry_interval = self.polling_interval();
        let retry_started_at = Instant::now();
        loop {
            match self.find_latest_block_number().await {
                Ok(Some(block_number)) => return block_number,
                Ok(None) => {
                    warn!(
                        ?retry_interval,
                        "No latest block available from block source; retrying"
                    );
                }
                Err(err) => {
                    warn!(
                        ?retry_interval,
                        ?err,
                        "Failed to find latest block number from block source; retrying"
                    );
                }
            }

            tokio::time::sleep(retry_interval).await;
            if retry_started_at.elapsed() >= LATEST_BLOCK_RETRY_BACKOFF_AFTER {
                retry_interval = retry_interval
                    .saturating_mul(2)
                    .min(LATEST_BLOCK_RETRY_MAX_INTERVAL);
            } else {
                retry_interval = polling_interval;
            }
        }
    }

    pub fn polling_interval(&self) -> Duration {
        self.source.polling_interval()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        node::types::BlockAndReceipts,
        pseudo_peer::sources::{BlockSource, BlockSourceBoxed},
    };
    use futures::{FutureExt, future::BoxFuture};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct EventuallyLatestBlockSource {
        attempts: Arc<AtomicUsize>,
    }

    impl BlockSource for EventuallyLatestBlockSource {
        fn collect_block(&self, _height: u64) -> BoxFuture<'static, eyre::Result<BlockAndReceipts>> {
            async { Err(eyre::eyre!("unused in this test")) }.boxed()
        }

        fn find_latest_block_number(&self) -> BoxFuture<'static, eyre::Result<Option<u64>>> {
            let attempts = self.attempts.clone();
            async move {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                match attempt {
                    0 => Err(eyre::eyre!("temporary source error")),
                    1 => Ok(None),
                    _ => Ok(Some(42)),
                }
            }
            .boxed()
        }

        fn recommended_chunk_size(&self) -> u64 {
            1
        }

        fn polling_interval(&self) -> Duration {
            Duration::from_millis(1)
        }
    }

    #[tokio::test]
    async fn wait_for_latest_block_number_retries_until_available() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let source: BlockSourceBoxed = Arc::new(Box::new(EventuallyLatestBlockSource {
            attempts: attempts.clone(),
        }));
        let store = BlockStore::new(source, None, 999);

        let latest = store.wait_for_latest_block_number().await;

        assert_eq!(latest, 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }
}
