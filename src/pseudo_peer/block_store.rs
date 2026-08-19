use super::{patch::testnet_gap_blocks, sources::BlockSourceBoxed, utils::LruBiMap};
use crate::node::types::BlockAndReceipts;
use alloy_primitives::B256;
use futures::future::BoxFuture;
use parking_lot::RwLock;
use reth_network::cache::LruMap;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};
use tracing::info;

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
    /// Block content cache keyed by the block's actual identity.
    blocks: RwLock<LruMap<B256, BlockAndReceipts>>,
    /// Current source-canonical hash at each cached height.
    canonical_hashes: RwLock<BTreeMap<u64, B256>>,
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
            canonical_hashes: RwLock::new(BTreeMap::new()),
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

    fn cache_block(&self, block: BlockAndReceipts) {
        let number = block.number();
        let hash = block.hash();
        if self.canonical_hashes.read().get(&number).is_some_and(|old_hash| *old_hash != hash) {
            self.invalidate_above(number.saturating_sub(1));
        }
        self.canonical_hashes.write().insert(number, hash);
        self.blocks.write().insert(hash, block.clone());
        self.index_block(&block);
    }

    pub fn invalidate_above(&self, block: u64) {
        let removed = self.canonical_hashes.write().split_off(&block.saturating_add(1));
        let mut blocks = self.blocks.write();
        let mut index = self.hash_index.write();
        for hash in removed.into_values() {
            blocks.remove(&hash);
            index.remove_by_left(&hash);
        }
    }

    /// Fetch a single block by number. Auto-indexes and caches.
    pub async fn get_by_number(&self, n: u64) -> eyre::Result<BlockAndReceipts> {
        if let Some(hash) = self.canonical_hashes.read().get(&n).copied()
            && let Some(block) = self.blocks.write().get(&hash)
        {
            return Ok(block.clone());
        }
        if let Some(block) = self.gap_blocks.get(&n) {
            let block = block.clone();
            self.cache_block(block.clone());
            return Ok(block);
        }
        let block = self.source.collect_block(n).await?;
        eyre::ensure!(
            block.number() == n,
            "Source returned block {} for requested height {n}",
            block.number()
        );
        self.cache_block(block.clone());
        Ok(block)
    }

    /// Bypasses source-local caches and replaces the canonical entry if it changed.
    pub async fn refresh_by_number(&self, n: u64) -> eyre::Result<(BlockAndReceipts, bool)> {
        let block = self.source.refresh_block(n).await?;
        eyre::ensure!(
            block.number() == n,
            "Source returned block {} for requested height {n}",
            block.number()
        );
        let changed =
            self.canonical_hashes.read().get(&n).is_some_and(|hash| *hash != block.hash());
        self.cache_block(block.clone());
        Ok((block, changed))
    }

    pub fn get_by_hash(&self, hash: B256) -> eyre::Result<BlockAndReceipts> {
        let block = self
            .blocks
            .write()
            .get(&hash)
            .cloned()
            .ok_or_else(|| eyre::eyre!("Block not cached for hash: {hash:?}"))?;
        eyre::ensure!(block.hash() == hash, "Cached block hash mismatch");
        Ok(block)
    }

    /// Fetch multiple blocks by number. Auto-indexes and caches.
    pub async fn get_by_numbers(&self, heights: Vec<u64>) -> eyre::Result<Vec<BlockAndReceipts>> {
        let mut cached: HashMap<u64, BlockAndReceipts> = HashMap::new();
        let mut uncached_heights = Vec::new();
        {
            for &h in &heights {
                let hash = self.canonical_hashes.read().get(&h).copied();
                if let Some(block) = hash.and_then(|hash| self.blocks.write().get(&hash).cloned()) {
                    cached.insert(h, block.clone());
                } else if let Some(block) = self.gap_blocks.get(&h) {
                    cached.insert(h, block.clone());
                } else {
                    uncached_heights.push(h);
                }
            }
        }

        if !uncached_heights.is_empty() {
            let fetched = self.source.collect_blocks(uncached_heights.clone()).await?;
            for block in fetched {
                let h = block.number();
                eyre::ensure!(
                    uncached_heights.contains(&h),
                    "Source returned unrequested block {h}"
                );
                self.cache_block(block.clone());
                cached.insert(h, block);
            }
        }

        for block in cached.values() {
            self.cache_block(block.clone());
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
            && let Some(n) = db_fn(hash)
        {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pseudo_peer::sources::test_utils;

    #[test]
    fn indexing_child_makes_parent_hash_resolvable() {
        let parent_hash = B256::repeat_byte(7);
        let child = test_utils::block_with_parent(42, 8, parent_hash);
        let mut index = LruBiMap::new(4);

        BlockStore::index_block_inner(&mut index, &child);

        assert_eq!(index.get_by_left(&child.hash()), Some(&42));
        assert_eq!(index.get_by_left(&parent_hash), Some(&41));
    }
}
