use super::{sources::BlockSource, utils::LruBiMap};
use crate::{
    chainspec::HlChainSpec,
    node::{
        network::{HlNetworkPrimitives, HlNewBlock},
        types::BlockAndReceipts,
    },
};
use alloy_eips::HashOrNumber;
use alloy_primitives::{B256, U128};
use alloy_rpc_types::Block;
use futures::StreamExt as _;
use parking_lot::RwLock;
use rayon::prelude::*;
use reth_eth_wire::{
    BlockBodies, BlockHeaders, GetBlockBodies, GetBlockHeaders, HeadersDirection, NewBlock,
};
use reth_network::{
    eth_requests::IncomingEthRequest,
    import::{BlockImport, BlockImportEvent, BlockValidation, NewBlockEvent},
    message::NewBlockMessage,
};
use reth_network_peers::PeerId;
use std::{
    collections::{HashMap, HashSet},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::{debug, info};

/// A cache of block hashes to block numbers.
pub type BlockHashCache = Arc<RwLock<LruBiMap<B256, u64>>>;
const BLOCKHASH_CACHE_LIMIT: u32 = 1000000;

pub fn new_blockhash_cache() -> BlockHashCache {
    Arc::new(RwLock::new(LruBiMap::new(BLOCKHASH_CACHE_LIMIT)))
}

/// A block poller that polls blocks from `BlockSource` and sends them to the `block_tx`
#[derive(Debug)]
pub struct BlockPoller {
    chain_id: u64,
    block_rx: mpsc::Receiver<(u64, BlockAndReceipts)>,
    task: JoinHandle<eyre::Result<()>>,
    blockhash_cache: BlockHashCache,
}

impl BlockPoller {
    pub fn new_suspended<BS: BlockSource>(
        chain_id: u64,
        block_source: BS,
        blockhash_cache: BlockHashCache,
        debug_cutoff_height: Option<u64>,
    ) -> (Self, mpsc::Sender<()>) {
        let block_source = Arc::new(block_source);
        let (start_tx, start_rx) = mpsc::channel(1);
        let (block_tx, block_rx) = mpsc::channel(100);
        let task = tokio::spawn(Self::task(start_rx, block_source, block_tx, debug_cutoff_height));
        (Self { chain_id, block_rx, task, blockhash_cache: blockhash_cache.clone() }, start_tx)
    }

    #[allow(unused)]
    pub fn task_handle(&self) -> &JoinHandle<eyre::Result<()>> {
        &self.task
    }

    async fn task<BS: BlockSource>(
        mut start_rx: mpsc::Receiver<()>,
        block_source: Arc<BS>,
        block_tx: mpsc::Sender<(u64, BlockAndReceipts)>,
        debug_cutoff_height: Option<u64>,
    ) -> eyre::Result<()> {
        start_rx.recv().await.ok_or(eyre::eyre!("Failed to receive start signal"))?;
        info!("Starting block poller");

        let polling_interval = block_source.polling_interval();
        let mut next_block_number = block_source
            .find_latest_block_number()
            .await
            .ok_or(eyre::eyre!("Failed to find latest block number"))?;

        if let Some(debug_cutoff_height) = debug_cutoff_height {
            if next_block_number > debug_cutoff_height {
                next_block_number = debug_cutoff_height;
            }
        }

        loop {
            match block_source.collect_block(next_block_number).await {
                Ok(block) => {
                    block_tx.send((next_block_number, block)).await?;
                    next_block_number += 1;
                }
                Err(_) => tokio::time::sleep(polling_interval).await,
            }
        }
    }
}

impl BlockImport<HlNewBlock> for BlockPoller {
    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<BlockImportEvent<HlNewBlock>> {
        debug!("(receiver) Polling");
        match Pin::new(&mut self.block_rx).poll_recv(_cx) {
            Poll::Ready(Some((number, block))) => {
                debug!("Polled block: {}", number);
                let reth_block = block.to_reth_block(self.chain_id);
                let hash = reth_block.header.hash_slow();
                self.blockhash_cache.write().insert(hash, number);
                let td = U128::from(reth_block.header.difficulty);
                Poll::Ready(BlockImportEvent::Announcement(BlockValidation::ValidHeader {
                    block: NewBlockMessage {
                        block: HlNewBlock(NewBlock { block: reth_block, td }).into(),
                        hash,
                    },
                }))
            }
            Poll::Ready(None) | Poll::Pending => Poll::Pending,
        }
    }

    fn on_new_block(&mut self, _peer_id: PeerId, _incoming_block: NewBlockEvent<HlNewBlock>) {}
}

/// A pseudo peer that can process eth requests and feed blocks to reth
pub struct PseudoPeer<BS: BlockSource> {
    chain_spec: Arc<HlChainSpec>,
    block_source: BS,
    blockhash_cache: BlockHashCache,
    warm_cache_size: u64,
    if_hit_then_warm_around: Arc<Mutex<HashSet<u64>>>,

    /// This is used to avoid calling `find_latest_block_number` too often.
    /// Only used for cache warmup.
    known_latest_block_number: u64,
}

impl<BS: BlockSource> PseudoPeer<BS> {
    pub fn new(
        chain_spec: Arc<HlChainSpec>,
        block_source: BS,
        blockhash_cache: BlockHashCache,
    ) -> Self {
        Self {
            chain_spec,
            block_source,
            blockhash_cache,
            warm_cache_size: 1000, // reth default chunk size for GetBlockBodies
            if_hit_then_warm_around: Arc::new(Mutex::new(HashSet::new())),
            known_latest_block_number: 0,
        }
    }

    async fn collect_block(&self, height: u64) -> eyre::Result<BlockAndReceipts> {
        self.block_source.collect_block(height).await
    }

    async fn collect_blocks(
        &self,
        block_numbers: impl IntoIterator<Item = u64>,
    ) -> eyre::Result<Vec<BlockAndReceipts>> {
        let block_numbers = block_numbers.into_iter().collect::<Vec<_>>();
        let res = futures::stream::iter(block_numbers)
            .map(async |number| self.collect_block(number).await)
            .buffered(self.block_source.recommended_chunk_size() as usize)
            .collect::<Vec<_>>()
            .await;
        res.into_iter().collect()
    }

    pub async fn process_eth_request(
        &mut self,
        eth_req: IncomingEthRequest<HlNetworkPrimitives>,
    ) -> eyre::Result<()> {
        let chain_id = self.chain_spec.inner.chain().id();
        match eth_req {
            IncomingEthRequest::GetBlockHeaders {
                peer_id: _,
                request: GetBlockHeaders { start_block, limit, skip, direction },
                response,
            } => {
                debug!(
                    "GetBlockHeaders request: {start_block:?}, {limit:?}, {skip:?}, {direction:?}"
                );
                let number = match start_block {
                    HashOrNumber::Hash(hash) => self.hash_to_block_number(hash).await,
                    HashOrNumber::Number(number) => number,
                };

                let block_headers = match direction {
                    HeadersDirection::Rising => self.collect_blocks(number..number + limit).await,
                    HeadersDirection::Falling => {
                        self.collect_blocks((number + 1 - limit..number + 1).rev()).await
                    }
                }?
                .into_par_iter()
                .map(|block| block.to_reth_block(chain_id).header.clone())
                .collect::<Vec<_>>();

                let _ = response.send(Ok(BlockHeaders(block_headers)));
            }
            IncomingEthRequest::GetBlockBodies { peer_id: _, request, response } => {
                let GetBlockBodies(hashes) = request;
                debug!("GetBlockBodies request: {}", hashes.len());

                let mut numbers = Vec::new();
                for hash in hashes {
                    numbers.push(self.hash_to_block_number(hash).await);
                }

                let block_bodies = self
                    .collect_blocks(numbers)
                    .await?
                    .into_iter()
                    .map(|block| block.to_reth_block(chain_id).body)
                    .collect::<Vec<_>>();

                let _ = response.send(Ok(BlockBodies(block_bodies)));
            }
            IncomingEthRequest::GetNodeData { .. } => debug!("GetNodeData request: {eth_req:?}"),
            eth_req => debug!("New eth protocol request: {eth_req:?}"),
        }
        Ok(())
    }

    async fn hash_to_block_number(&mut self, hash: B256) -> u64 {
        // First, try to find the hash in our cache
        if let Some(block_number) = self.try_get_cached_block_number(hash).await {
            return block_number;
        }

        let latest = self.block_source.find_latest_block_number().await.unwrap();
        self.known_latest_block_number = latest;

        // These constants are quite arbitrary but works well in practice
        const BACKFILL_RETRY_LIMIT: u64 = 10;

        for _ in 0..BACKFILL_RETRY_LIMIT {
            // If not found, backfill the cache and retry
            if let Ok(Some(block_number)) = self.backfill_cache_for_hash(hash, latest).await {
                return block_number;
            }
        }

        panic!("Hash not found: {hash:?}");
    }

    async fn fallback_to_official_rpc(&self, hash: B256) -> eyre::Result<u64> {
        // This is tricky because Raw EVM files (BlockSource) does not have hash to number mapping
        // so we can either enumerate all blocks to get hash to number mapping, or fallback to an
        // official RPC. The latter is much easier but has 300/day rate limit.
        use jsonrpsee::http_client::HttpClientBuilder;
        use jsonrpsee_core::client::ClientT;

        debug!("Fallback to official RPC: {hash:?}");
        let client =
            HttpClientBuilder::default().build(self.chain_spec.official_rpc_url()).unwrap();
        let target_block: Block = client.request("eth_getBlockByHash", (hash, false)).await?;
        debug!("From official RPC: {:?} for {hash:?}", target_block.header.number);
        self.cache_blocks([(hash, target_block.header.number)]);
        Ok(target_block.header.number)
    }

    /// Try to get a block number from the cache for the given hash
    async fn try_get_cached_block_number(&mut self, hash: B256) -> Option<u64> {
        let maybe_block_number = self.blockhash_cache.read().get_by_left(&hash).copied();
        if let Some(block_number) = maybe_block_number {
            if self.if_hit_then_warm_around.lock().unwrap().contains(&block_number) {
                self.warm_cache_around_blocks(block_number, self.warm_cache_size).await;
            }
            Some(block_number)
        } else {
            None
        }
    }

    /// Backfill the cache with blocks to find the target hash
    async fn backfill_cache_for_hash(
        &mut self,
        target_hash: B256,
        latest: u64,
    ) -> eyre::Result<Option<u64>> {
        let chunk_size = self.block_source.recommended_chunk_size();
        debug!("Hash not found, backfilling... {target_hash:?}");

        const TRY_OFFICIAL_RPC_THRESHOLD: usize = 20;
        for (iteration, end) in (1..=latest).rev().step_by(chunk_size as usize).enumerate() {
            // Calculate the range to backfill
            let start = std::cmp::max(end.saturating_sub(chunk_size), 1);

            // Backfill this chunk
            if let Ok(Some(block_number)) =
                self.try_block_range_for_hash(start, end, target_hash).await
            {
                return Ok(Some(block_number));
            }

            // If not found, first fallback to an official RPC
            if iteration >= TRY_OFFICIAL_RPC_THRESHOLD {
                match self.fallback_to_official_rpc(target_hash).await {
                    Ok(block_number) => {
                        self.warm_cache_around_blocks(block_number, self.warm_cache_size).await;
                        return Ok(Some(block_number));
                    }
                    Err(e) => {
                        debug!("Fallback to official RPC failed: {e:?}");
                    }
                }
            }
        }

        debug!("Hash not found: {target_hash:?}, retrying from the latest block...");
        Ok(None) // Not found
    }

    async fn warm_cache_around_blocks(&mut self, block_number: u64, chunk_size: u64) {
        let start = std::cmp::max(block_number.saturating_sub(chunk_size), 1);
        let end = std::cmp::min(block_number + chunk_size, self.known_latest_block_number);
        {
            let mut guard = self.if_hit_then_warm_around.lock().unwrap();
            guard.insert(start);
            guard.insert(end);
        }
        const IMPOSSIBLE_HASH: B256 = B256::ZERO;
        let _ = self.try_block_range_for_hash(start, end, IMPOSSIBLE_HASH).await;
    }

    /// Backfill a specific range of block numbers into the cache
    async fn try_block_range_for_hash(
        &mut self,
        start_number: u64,
        end_number: u64,
        target_hash: B256,
    ) -> eyre::Result<Option<u64>> {
        // Get block numbers that are already cached
        let (cached_block_hashes, uncached_block_numbers) =
            self.get_cached_block_hashes(start_number, end_number);

        if let Some(&block_number) = cached_block_hashes.get(&target_hash) {
            return Ok(Some(block_number));
        }

        if uncached_block_numbers.is_empty() {
            debug!("All blocks are cached, returning None");
            return Ok(None);
        }

        debug!("Backfilling from {start_number} to {end_number}");
        // Collect blocks and cache them
        let blocks = self.collect_blocks(uncached_block_numbers).await?;
        let block_map: HashMap<B256, u64> =
            blocks.into_iter().map(|block| (block.hash(), block.number())).collect();
        let maybe_block_number = block_map.get(&target_hash).copied();
        self.cache_blocks(block_map);
        Ok(maybe_block_number)
    }

    /// Get block numbers in the range that are already cached
    fn get_cached_block_hashes(
        &self,
        start_number: u64,
        end_number: u64,
    ) -> (HashMap<B256, u64>, Vec<u64>) {
        let map = self.blockhash_cache.read();
        let (cached, uncached): (Vec<u64>, Vec<u64>) =
            (start_number..=end_number).partition(|number| map.get_by_right(number).is_some());
        let cached_block_hashes = cached
            .into_iter()
            .filter_map(|number| map.get_by_right(&number).map(|&hash| (hash, number)))
            .collect();
        (cached_block_hashes, uncached)
    }

    /// Cache a collection of blocks in the hash-to-number mapping
    fn cache_blocks(&self, blocks: impl IntoIterator<Item = (B256, u64)>) {
        let mut map = self.blockhash_cache.write();
        for (hash, number) in blocks {
            map.insert(hash, number);
        }
    }
}
