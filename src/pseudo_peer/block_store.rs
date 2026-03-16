use super::{sources::BlockSourceBoxed, utils::LruBiMap};
use crate::node::types::BlockAndReceipts;
use alloy_primitives::B256;
use futures::future::BoxFuture;
use parking_lot::RwLock;
use reth_network::cache::LruMap;
use std::{
    collections::HashMap,
    sync::Arc,
    time::Duration,
};

/// Function that resolves a block hash to its number via the node's database.
/// Returns `None` if the hash is not yet in the database (e.g. headers not synced yet).
/// Note: This queries the HeaderNumbers table which covers both database and static files.
pub type DbBlockNumberFn = Arc<dyn Fn(B256) -> Option<u64> + Send + Sync>;

const BLOCK_CACHE_LIMIT: u32 = 100_000;
const HASH_INDEX_LIMIT: u32 = 1_000_000;

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
}

impl std::fmt::Debug for BlockStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockStore").finish_non_exhaustive()
    }
}

impl BlockStore {
    pub fn new(source: BlockSourceBoxed, db_block_number: Option<DbBlockNumberFn>) -> Self {
        Self {
            blocks: RwLock::new(LruMap::new(BLOCK_CACHE_LIMIT)),
            hash_index: RwLock::new(LruBiMap::new(HASH_INDEX_LIMIT)),
            db_block_number,
            source,
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

    pub fn find_latest_block_number(&self) -> BoxFuture<'static, Option<u64>> {
        self.source.find_latest_block_number()
    }

    pub fn polling_interval(&self) -> Duration {
        self.source.polling_interval()
    }
}
