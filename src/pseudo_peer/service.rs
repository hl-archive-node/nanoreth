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
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::{debug, info, warn};

/// A cache of block hashes to block numbers.
pub type BlockHashCache = Arc<RwLock<LruBiMap<B256, u64>>>;
const BLOCKHASH_CACHE_LIMIT: u32 = 1_000_000;

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

        loop {
            if let Some(debug_cutoff_height) = debug_cutoff_height &&
                next_block_number > debug_cutoff_height
            {
                next_block_number = debug_cutoff_height;
            }

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

/// Function that resolves a block hash to its number via the node's database.
/// Returns `None` if the hash is not yet in the database (e.g. headers not synced yet).
pub type DbBlockNumberFn = Arc<dyn Fn(B256) -> Option<u64> + Send + Sync>;

/// A pseudo peer that can process eth requests and feed blocks to reth
pub struct PseudoPeer<BS: BlockSource> {
    chain_spec: Arc<HlChainSpec>,
    block_source: BS,
    blockhash_cache: BlockHashCache,

    /// Database lookup for hash→number resolution.
    /// Reads directly from the node's header database, avoiding expensive
    /// block source scans.
    db_block_number: Option<DbBlockNumberFn>,
}

impl<BS: BlockSource> PseudoPeer<BS> {
    pub fn new(
        chain_spec: Arc<HlChainSpec>,
        block_source: BS,
        blockhash_cache: BlockHashCache,
        db_block_number: Option<DbBlockNumberFn>,
    ) -> Self {
        Self {
            chain_spec,
            block_source,
            blockhash_cache,
            db_block_number,
        }
    }

    async fn collect_blocks(
        &self,
        block_numbers: impl IntoIterator<Item = u64>,
    ) -> eyre::Result<Vec<BlockAndReceipts>> {
        let block_numbers = block_numbers.into_iter().collect::<Vec<_>>();
        self.block_source.collect_blocks(block_numbers).await
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
                    HashOrNumber::Hash(hash) => match self.hash_to_block_number(hash).await {
                        Ok(n) => n,
                        Err(e) => {
                            warn!("Failed to resolve block hash {hash:?}: {e}");
                            let _ = response.send(Ok(BlockHeaders(vec![])));
                            return Ok(());
                        }
                    },
                    HashOrNumber::Number(number) => number,
                };

                let blocks = match direction {
                    HeadersDirection::Rising => self.collect_blocks(number..number + limit).await,
                    HeadersDirection::Falling => {
                        self.collect_blocks((number + 1 - limit..number + 1).rev()).await
                    }
                }?;

                // Cache hash→number mappings so the Bodies stage can resolve them later
                let block_headers: Vec<_> = blocks
                    .into_par_iter()
                    .map(|block| {
                        let number = block.number();
                        let reth_block = block.to_reth_block(chain_id);
                        let hash = reth_block.header.hash_slow();
                        (hash, number, reth_block.header.clone())
                    })
                    .collect();

                self.cache_blocks(block_headers.iter().map(|(hash, number, _)| (*hash, *number)));

                let headers = block_headers.into_iter().map(|(_, _, h)| h).collect();
                let _ = response.send(Ok(BlockHeaders(headers)));
            }
            IncomingEthRequest::GetBlockBodies { peer_id: _, request, response } => {
                let GetBlockBodies(hashes) = request;
                debug!("GetBlockBodies request: {}", hashes.len());

                let mut numbers = Vec::new();
                for hash in hashes {
                    match self.hash_to_block_number(hash).await {
                        Ok(n) => numbers.push(n),
                        Err(e) => warn!("Failed to resolve block hash {hash:?}: {e}"),
                    }
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

    async fn hash_to_block_number(&self, hash: B256) -> eyre::Result<u64> {
        // Fast path: check in-memory cache
        if let Some(block_number) = self.blockhash_cache.read().get_by_left(&hash).copied() {
            return Ok(block_number);
        }

        // Look up in the node's database (all headers are stored after the Headers stage)
        if let Some(ref db_fn) = self.db_block_number
            && let Some(block_number) = db_fn(hash)
        {
            self.cache_blocks([(hash, block_number)]);
            return Ok(block_number);
        }

        Err(eyre::eyre!("Hash not found in cache or database: {hash:?}"))
    }

    /// Cache a collection of blocks in the hash-to-number mapping
    fn cache_blocks(&self, blocks: impl IntoIterator<Item = (B256, u64)>) {
        let mut map = self.blockhash_cache.write();
        for (hash, number) in blocks {
            map.insert(hash, number);
        }
    }
}
