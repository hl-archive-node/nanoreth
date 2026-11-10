// jsonrpsee's `#[rpc]` expansion adds `#[must_use]` to methods whose boxed future already is.
#![allow(clippy::double_must_use)]

use crate::node::types::BlockAndReceipts;
use alloy_primitives::Bytes;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee_core::{RpcResult, async_trait};
use reth::rpc::result::internal_rpc_err;
use std::sync::OnceLock;
use tracing::trace;

/// Trait for reading blocks from the database for the sync server.
pub trait SyncBlockReader: Send + Sync + 'static {
    fn read_block_and_receipts(&self, number: u64) -> eyre::Result<BlockAndReceipts>;
    fn best_block_number(&self) -> eyre::Result<u64>;
}

/// Wraps any reth provider that implements the needed traits.
pub struct ProviderSyncReader<P> {
    provider: P,
}

impl<P> ProviderSyncReader<P> {
    pub fn new(provider: P) -> Self {
        Self { provider }
    }
}

impl<P> SyncBlockReader for ProviderSyncReader<P>
where
    P: reth_provider::BlockReader<Block = crate::HlBlock>
        + reth_provider::ReceiptProvider<Receipt = reth_ethereum_primitives::EthereumReceipt>
        + reth_provider::BlockNumReader
        + Send
        + Sync
        + 'static,
{
    fn read_block_and_receipts(&self, number: u64) -> eyre::Result<BlockAndReceipts> {
        let block = self
            .provider
            .block_by_number(number)?
            .ok_or_else(|| eyre::eyre!("Block {number} not found in database"))?;
        let receipts = self
            .provider
            .receipts_by_block(number.into())?
            .ok_or_else(|| eyre::eyre!("Receipts for block {number} not found in database"))?;
        Ok(BlockAndReceipts::from_db(block, receipts))
    }

    fn best_block_number(&self) -> eyre::Result<u64> {
        Ok(self.provider.last_block_number()?)
    }
}

static DB_READER: OnceLock<Box<dyn SyncBlockReader>> = OnceLock::new();

/// Set the database reader for the sync server.
/// Called during node startup when `--enable-sync-server` is set.
pub fn set_sync_db_reader(reader: Box<dyn SyncBlockReader>) {
    DB_READER.set(reader).ok();
}

fn get_sync_db_reader() -> RpcResult<&'static dyn SyncBlockReader> {
    DB_READER
        .get()
        .map(|b| b.as_ref())
        .ok_or_else(|| internal_rpc_err("Sync server not yet initialized"))
}

/// RPC trait for node-to-node block syncing.
///
/// Serves blocks directly from the database so other nanoreth nodes
/// can sync without needing direct S3 access.
#[rpc(server, namespace = "hl")]
#[async_trait]
pub trait HlSyncApi {
    /// Returns a block at the given height, serialized as msgpack+lz4 bytes.
    #[method(name = "syncGetBlock")]
    async fn sync_get_block(&self, height: u64) -> RpcResult<Bytes>;

    /// Returns multiple blocks by height, serialized as msgpack+lz4 bytes.
    /// Heights are capped at 500 per request.
    #[method(name = "syncGetBlocks")]
    async fn sync_get_blocks(&self, heights: Vec<u64>) -> RpcResult<Bytes>;

    /// Returns the latest block number available from this node's database.
    #[method(name = "syncLatestBlockNumber")]
    async fn sync_latest_block_number(&self) -> RpcResult<Option<u64>>;
}

pub struct HlSyncServer;

#[async_trait]
impl HlSyncApiServer for HlSyncServer {
    async fn sync_get_block(&self, height: u64) -> RpcResult<Bytes> {
        trace!(target: "rpc::hl", height, "Serving hl_syncGetBlock");
        let reader = get_sync_db_reader()?;
        let block = reader
            .read_block_and_receipts(height)
            .map_err(|e| internal_rpc_err(format!("Failed to read block {height}: {e}")))?;

        // Encode as msgpack + lz4 (same format as S3/local block sources).
        // Use write_named (map format) to match the S3/Go msgpack format.
        let mut encoder = lz4_flex::frame::FrameEncoder::new(Vec::new());
        rmp_serde::encode::write_named(&mut encoder, &vec![block])
            .map_err(|e| internal_rpc_err(format!("Failed to serialize block: {e}")))?;
        let compressed = encoder
            .finish()
            .map_err(|e| internal_rpc_err(format!("Failed to compress block: {e}")))?;
        Ok(Bytes::from(compressed))
    }

    async fn sync_get_blocks(&self, heights: Vec<u64>) -> RpcResult<Bytes> {
        const MAX_BATCH: usize = 500;
        let heights = if heights.len() > MAX_BATCH { &heights[..MAX_BATCH] } else { &heights };
        trace!(target: "rpc::hl", count = heights.len(), "Serving hl_syncGetBlocks");
        let reader = get_sync_db_reader()?;

        let blocks: Vec<BlockAndReceipts> = heights
            .iter()
            .map(|&h| reader.read_block_and_receipts(h))
            .collect::<Result<_, _>>()
            .map_err(|e| internal_rpc_err(format!("Failed to read blocks: {e}")))?;

        let mut encoder = lz4_flex::frame::FrameEncoder::new(Vec::new());
        rmp_serde::encode::write_named(&mut encoder, &blocks)
            .map_err(|e| internal_rpc_err(format!("Failed to serialize blocks: {e}")))?;
        let compressed = encoder
            .finish()
            .map_err(|e| internal_rpc_err(format!("Failed to compress blocks: {e}")))?;
        Ok(Bytes::from(compressed))
    }

    async fn sync_latest_block_number(&self) -> RpcResult<Option<u64>> {
        trace!(target: "rpc::hl", "Serving hl_syncLatestBlockNumber");
        let reader = get_sync_db_reader()?;
        Ok(Some(
            reader
                .best_block_number()
                .map_err(|e| internal_rpc_err(format!("Failed to get latest block: {e}")))?,
        ))
    }
}
