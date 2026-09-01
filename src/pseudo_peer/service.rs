use super::block_store::BlockStore;
use crate::{
    chainspec::HlChainSpec,
    node::{
        network::{HlNetworkPrimitives, HlNewBlock},
        types::BlockAndReceipts,
    },
};
use alloy_eips::HashOrNumber;
use alloy_primitives::U128;
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
use std::time::{Duration, Instant};
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::{debug, info, warn};

/// A block poller that polls blocks from `BlockStore` and sends them to the `block_tx`
#[derive(Debug)]
pub struct BlockPoller {
    chain_id: u64,
    block_rx: mpsc::Receiver<(u64, BlockAndReceipts)>,
    task: JoinHandle<eyre::Result<()>>,
    block_store: Arc<BlockStore>,
}

impl BlockPoller {
    pub fn new_suspended(
        chain_id: u64,
        block_store: Arc<BlockStore>,
        debug_cutoff_height: Option<u64>,
    ) -> (Self, mpsc::Sender<()>) {
        let (start_tx, start_rx) = mpsc::channel(1);
        let (block_tx, block_rx) = mpsc::channel(100);
        let store = block_store.clone();
        let task = tokio::spawn(Self::task(start_rx, store, block_tx, debug_cutoff_height));
        (Self { chain_id, block_rx, task, block_store }, start_tx)
    }

    #[allow(unused)]
    pub fn task_handle(&self) -> &JoinHandle<eyre::Result<()>> {
        &self.task
    }

    async fn task(
        mut start_rx: mpsc::Receiver<()>,
        block_store: Arc<BlockStore>,
        block_tx: mpsc::Sender<(u64, BlockAndReceipts)>,
        debug_cutoff_height: Option<u64>,
    ) -> eyre::Result<()> {
        start_rx.recv().await.ok_or(eyre::eyre!("Failed to receive start signal"))?;
        info!("Starting block poller");

        let polling_interval = block_store.polling_interval();
        let mut next_reorg_check = Instant::now();
        let mut next_block_number = block_store
            .find_latest_block_number()
            .await
            .ok_or(eyre::eyre!("Failed to find latest block number"))?;

        loop {
            if let Some(debug_cutoff_height) = debug_cutoff_height
                && next_block_number > debug_cutoff_height
            {
                next_block_number = debug_cutoff_height;
            }

            if next_block_number > 0 && Instant::now() >= next_reorg_check {
                next_reorg_check = Instant::now() + Duration::from_secs(1);
                let previous = next_block_number - 1;
                if let Ok((refreshed, changed)) = block_store.refresh_by_number(previous).await {
                    let expected_parent = if previous > 0 {
                        block_store.get_by_number(previous - 1).await.ok().map(|block| block.hash())
                    } else {
                        None
                    };
                    if changed
                        || expected_parent.is_some_and(|hash| hash != refreshed.parent_hash())
                    {
                        block_store.invalidate_above(previous.saturating_sub(1));
                        next_block_number = previous;
                        continue;
                    }
                }
            }

            match block_store.get_by_number(next_block_number).await {
                Ok(block) => {
                    if next_block_number > 0
                        && block_store
                            .get_by_number(next_block_number - 1)
                            .await
                            .is_ok_and(|parent| parent.hash() != block.parent_hash())
                    {
                        block_store.invalidate_above(next_block_number.saturating_sub(2));
                        next_block_number -= 1;
                        continue;
                    }
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
                self.block_store.index_block(&block);
                let reth_block = block.to_reth_block(self.chain_id);
                let hash = reth_block.header.hash_slow();
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
pub struct PseudoPeer {
    chain_spec: Arc<HlChainSpec>,
    block_store: Arc<BlockStore>,
}

impl PseudoPeer {
    pub fn new(chain_spec: Arc<HlChainSpec>, block_store: Arc<BlockStore>) -> Self {
        Self { chain_spec, block_store }
    }

    async fn collect_blocks(
        &self,
        block_numbers: impl IntoIterator<Item = u64>,
    ) -> eyre::Result<Vec<BlockAndReceipts>> {
        let block_numbers = block_numbers.into_iter().collect::<Vec<_>>();
        self.block_store.get_by_numbers(block_numbers).await
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
                    HashOrNumber::Hash(hash) => match self.block_store.hash_to_number(hash) {
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

                // collect_blocks auto-indexes hashes via BlockStore
                let block_headers: Vec<_> = blocks
                    .into_par_iter()
                    .map(|block| {
                        let reth_block = block.to_reth_block(chain_id);
                        reth_block.header.clone()
                    })
                    .collect();

                let _ = response.send(Ok(BlockHeaders(block_headers)));
            }
            IncomingEthRequest::GetBlockBodies { peer_id: _, request, response } => {
                let GetBlockBodies(hashes) = request;
                debug!("GetBlockBodies request: {}", hashes.len());

                let mut blocks = Vec::new();
                for hash in hashes {
                    match self.block_store.get_by_hash(hash) {
                        Ok(block) => blocks.push(block),
                        Err(e) => warn!("Failed to resolve block hash {hash:?}: {e}"),
                    }
                }

                let block_bodies = blocks
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
}
