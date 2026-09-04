use super::{patch::testnet_gap_blocks, sources::BlockSourceBoxed, utils::LruBiMap};
use crate::node::types::BlockAndReceipts;
use alloy_primitives::B256;
use futures::future::BoxFuture;
use parking_lot::RwLock;
use reth_network::cache::LruMap;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
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
const HASH_RECOVERY_TIMEOUT: Duration = Duration::from_secs(30);

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

    /// Returns whether the exact block is still part of the source-canonical in-memory view.
    pub fn is_cached_canonical_hash(&self, hash: B256) -> bool {
        let Some(block) = self.blocks.write().get(&hash).cloned() else {
            return false;
        };
        self.canonical_hashes
            .read()
            .get(&block.number())
            .is_some_and(|canonical_hash| *canonical_hash == hash)
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

    pub async fn get_by_hash(&self, hash: B256) -> eyre::Result<BlockAndReceipts> {
        self.get_by_hashes(vec![hash]).await.pop().expect("one result per requested hash")
    }

    fn cache_recovered_block(
        &self,
        hash: B256,
        number: u64,
        block: BlockAndReceipts,
    ) -> eyre::Result<BlockAndReceipts> {
        eyre::ensure!(
            block.number() == number,
            "Source returned block {} for requested height {number}",
            block.number()
        );
        eyre::ensure!(
            block.hash() == hash,
            "Source returned canonical hash {:?} for requested hash {hash:?} at height {number}",
            block.hash()
        );
        {
            let mut canonical_hashes = self.canonical_hashes.write();
            if let Some(canonical_hash) = canonical_hashes.get(&number) {
                eyre::ensure!(
                    *canonical_hash == hash,
                    "Refusing stale block {hash:?} at height {number}; current canonical hash is {canonical_hash:?}"
                );
            } else {
                canonical_hashes.insert(number, hash);
            }
            self.blocks.write().insert(hash, block.clone());
            self.index_block(&block);
        }
        Ok(block)
    }

    /// Fetch blocks by hash while batching source requests and preserving request order.
    pub async fn get_by_hashes(&self, hashes: Vec<B256>) -> Vec<eyre::Result<BlockAndReceipts>> {
        let mut results = std::iter::repeat_with(|| None).take(hashes.len()).collect::<Vec<_>>();
        let mut missing = Vec::new();
        let mut heights = BTreeSet::new();

        for (index, hash) in hashes.into_iter().enumerate() {
            if let Some(block) = self.blocks.write().get(&hash).cloned() {
                results[index] = Some(if block.hash() == hash {
                    Ok(block)
                } else {
                    Err(eyre::eyre!("Cached block hash mismatch"))
                });
                continue;
            }

            match self.hash_to_number(hash) {
                Ok(number) => {
                    missing.push((index, hash, number));
                    if !self.gap_blocks.contains_key(&number) {
                        heights.insert(number);
                    }
                }
                Err(error) => results[index] = Some(Err(error)),
            }
        }

        let fetched = if heights.is_empty() {
            Ok(Vec::new())
        } else {
            tokio::time::timeout(
                HASH_RECOVERY_TIMEOUT,
                self.source.collect_blocks(heights.into_iter().collect()),
            )
            .await
            .map_err(|_| eyre::eyre!("Timed out recovering requested blocks"))
            .and_then(|result| result)
        };

        match fetched {
            Ok(blocks) => {
                let blocks_by_number = blocks
                    .into_iter()
                    .map(|block| (block.number(), block))
                    .collect::<HashMap<_, _>>();
                for (index, hash, number) in missing {
                    let block = self
                        .gap_blocks
                        .get(&number)
                        .cloned()
                        .or_else(|| blocks_by_number.get(&number).cloned());
                    results[index] = Some(match block {
                        Some(block) => self.cache_recovered_block(hash, number, block),
                        None => Err(eyre::eyre!("Block {number} not returned by source")),
                    });
                }
            }
            Err(error) => {
                let error = error.to_string();
                for (index, hash, number) in missing {
                    results[index] = Some(Err(eyre::eyre!(
                        "Failed recovering block {hash:?} at height {number}: {error}"
                    )));
                }
            }
        }

        results.into_iter().map(|result| result.expect("one result per requested hash")).collect()
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
    use crate::pseudo_peer::sources::{BlockSource, test_utils};
    use futures::FutureExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    #[derive(Debug)]
    struct MockSource(BlockAndReceipts);

    impl BlockSource for MockSource {
        fn collect_block(
            &self,
            _height: u64,
        ) -> BoxFuture<'static, eyre::Result<BlockAndReceipts>> {
            let block = self.0.clone();
            async move { Ok(block) }.boxed()
        }

        fn find_latest_block_number(&self) -> BoxFuture<'static, Option<u64>> {
            let number = self.0.number();
            async move { Some(number) }.boxed()
        }

        fn recommended_chunk_size(&self) -> u64 {
            1
        }
    }

    #[derive(Debug)]
    struct DelayedSource {
        block: BlockAndReceipts,
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[derive(Debug)]
    struct BatchSource {
        blocks: Vec<BlockAndReceipts>,
        batches: Arc<AtomicUsize>,
    }

    impl BlockSource for BatchSource {
        fn collect_block(
            &self,
            _height: u64,
        ) -> BoxFuture<'static, eyre::Result<BlockAndReceipts>> {
            async { Err(eyre::eyre!("unexpected single-block fetch")) }.boxed()
        }

        fn collect_blocks(
            &self,
            _heights: Vec<u64>,
        ) -> BoxFuture<'static, eyre::Result<Vec<BlockAndReceipts>>> {
            let blocks = self.blocks.clone();
            let batches = self.batches.clone();
            async move {
                batches.fetch_add(1, Ordering::SeqCst);
                Ok(blocks)
            }
            .boxed()
        }

        fn find_latest_block_number(&self) -> BoxFuture<'static, Option<u64>> {
            async { None }.boxed()
        }

        fn recommended_chunk_size(&self) -> u64 {
            1
        }
    }

    impl BlockSource for DelayedSource {
        fn collect_block(
            &self,
            _height: u64,
        ) -> BoxFuture<'static, eyre::Result<BlockAndReceipts>> {
            let block = self.block.clone();
            let started = self.started.clone();
            let release = self.release.clone();
            async move {
                started.notify_one();
                release.notified().await;
                Ok(block)
            }
            .boxed()
        }

        fn find_latest_block_number(&self) -> BoxFuture<'static, Option<u64>> {
            async { None }.boxed()
        }

        fn recommended_chunk_size(&self) -> u64 {
            1
        }
    }

    #[test]
    fn indexing_child_makes_parent_hash_resolvable() {
        let parent_hash = B256::repeat_byte(7);
        let child = test_utils::block_with_parent(42, 8, parent_hash);
        let mut index = LruBiMap::new(4);

        BlockStore::index_block_inner(&mut index, &child);

        assert_eq!(index.get_by_left(&child.hash()), Some(&42));
        assert_eq!(index.get_by_left(&parent_hash), Some(&41));
    }

    #[tokio::test]
    async fn hash_lookup_refetches_evicted_block_by_number() {
        let block = test_utils::block(42, 7);
        let source = Arc::new(Box::new(MockSource(block.clone())) as Box<dyn BlockSource>);
        let store = BlockStore::new(source, None, 998);
        store.cache_block(block.clone());
        store.blocks.write().remove(&block.hash());

        assert_eq!(store.get_by_hash(block.hash()).await.unwrap(), block);
    }

    #[tokio::test]
    async fn hash_lookups_use_one_batch_and_preserve_request_order() {
        let first = test_utils::block(41, 7);
        let second = test_utils::block(42, 8);
        let batches = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(Box::new(BatchSource {
            blocks: vec![first.clone(), second.clone()],
            batches: batches.clone(),
        }) as Box<dyn BlockSource>);
        let store = BlockStore::new(source, None, 998);
        store.index_block(&first);
        store.index_block(&second);

        let blocks = store
            .get_by_hashes(vec![second.hash(), first.hash()])
            .await
            .into_iter()
            .collect::<eyre::Result<Vec<_>>>()
            .unwrap();

        assert_eq!(batches.load(Ordering::SeqCst), 1);
        assert_eq!(blocks, vec![second, first]);
    }

    #[tokio::test]
    async fn hash_lookup_rejects_source_replacement() {
        let indexed = test_utils::block(42, 7);
        let replacement = test_utils::block_with_parent(42, 8, B256::repeat_byte(6));
        let source = Arc::new(Box::new(MockSource(replacement)) as Box<dyn BlockSource>);
        let store = BlockStore::new(source, None, 998);
        store.index_block(&indexed);

        assert!(store.get_by_hash(indexed.hash()).await.is_err());
    }

    #[tokio::test]
    async fn late_hash_recovery_cannot_replace_refreshed_canonical_block() {
        let old = test_utils::block(42, 7);
        let old_hash = old.hash();
        let replacement = test_utils::block(42, 8);
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let source = Arc::new(Box::new(DelayedSource {
            block: old.clone(),
            started: started.clone(),
            release: release.clone(),
        }) as Box<dyn BlockSource>);
        let store = Arc::new(BlockStore::new(source, None, 998));
        store.index_block(&old);

        let recovering = {
            let store = store.clone();
            tokio::spawn(async move { store.get_by_hash(old_hash).await })
        };
        started.notified().await;
        store.cache_block(replacement.clone());
        release.notify_one();

        assert!(recovering.await.unwrap().is_err());
        assert!(store.is_cached_canonical_hash(replacement.hash()));
        assert!(!store.is_cached_canonical_hash(old_hash));
    }

    #[tokio::test]
    async fn stale_database_hash_is_not_a_valid_queued_announcement() {
        let old = test_utils::block(42, 7);
        let replacement = test_utils::block_with_parent(42, 8, B256::repeat_byte(6));
        let old_hash = old.hash();
        let source = Arc::new(Box::new(MockSource(replacement.clone())) as Box<dyn BlockSource>);
        let db_block_number: DbBlockNumberFn =
            Arc::new(move |hash| (hash == old_hash).then_some(42));
        let store = BlockStore::new(source, Some(db_block_number), 998);
        store.cache_block(replacement);

        assert_eq!(store.hash_to_number(old_hash).unwrap(), 42);
        assert!(!store.is_cached_canonical_hash(old_hash));
    }
}
