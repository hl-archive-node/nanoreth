use crate::{HlBlock, HlPrimitives, node::types::BlockAndReceipts};
use alloy_primitives::{Bytes, Sealable};
use jsonrpsee::proc_macros::rpc;
use jsonrpsee_core::{RpcResult, async_trait};
use reth::rpc::result::internal_rpc_err;
use reth_ethereum_primitives::EthereumReceipt;
use reth_provider::{
    BlockNumReader, BlockReader, ProviderResult, ReceiptProvider,
    providers::{BlockchainProvider, ConsistentProvider, ProviderNodeTypes},
};
use std::sync::OnceLock;
use tracing::trace;

/// Trait for reading blocks from the database for the sync server.
pub trait SyncBlockReader: Send + Sync + 'static {
    fn read_blocks_and_receipts(&self, numbers: &[u64]) -> eyre::Result<Vec<BlockAndReceipts>>;
    fn best_block_number(&self) -> eyre::Result<u64>;
}

trait ConsistentSyncProvider: Send + Sync + 'static {
    type Snapshot: SyncSnapshot;

    fn consistent_snapshot(&self) -> ProviderResult<Self::Snapshot>;
    fn last_block_number(&self) -> ProviderResult<u64>;
}

trait SyncSnapshot {
    fn block_by_number(&self, number: u64) -> ProviderResult<Option<HlBlock>>;
    fn receipts_by_block_hash(
        &self,
        hash: alloy_primitives::B256,
    ) -> ProviderResult<Option<Vec<EthereumReceipt>>>;
}

impl<T> SyncSnapshot for T
where
    T: BlockReader<Block = HlBlock> + ReceiptProvider<Receipt = EthereumReceipt>,
{
    fn block_by_number(&self, number: u64) -> ProviderResult<Option<HlBlock>> {
        BlockReader::block_by_number(self, number)
    }

    fn receipts_by_block_hash(
        &self,
        hash: alloy_primitives::B256,
    ) -> ProviderResult<Option<Vec<EthereumReceipt>>> {
        ReceiptProvider::receipts_by_block(self, hash.into())
    }
}

impl<N> ConsistentSyncProvider for BlockchainProvider<N>
where
    N: ProviderNodeTypes<Primitives = HlPrimitives> + 'static,
{
    type Snapshot = ConsistentProvider<N>;

    fn consistent_snapshot(&self) -> ProviderResult<Self::Snapshot> {
        self.consistent_provider()
    }

    fn last_block_number(&self) -> ProviderResult<u64> {
        BlockNumReader::last_block_number(self)
    }
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
    P: ConsistentSyncProvider,
{
    fn read_blocks_and_receipts(&self, numbers: &[u64]) -> eyre::Result<Vec<BlockAndReceipts>> {
        let snapshot = self.provider.consistent_snapshot()?;
        numbers
            .iter()
            .map(|&number| {
                let block = snapshot
                    .block_by_number(number)?
                    .ok_or_else(|| eyre::eyre!("Block {number} not found in database"))?;
                let hash = block.header.hash_slow();
                let receipts = snapshot.receipts_by_block_hash(hash)?.ok_or_else(|| {
                    eyre::eyre!("Receipts for block {number} ({hash}) not found in database")
                })?;
                BlockAndReceipts::from_db(block, receipts)
            })
            .collect()
    }

    fn best_block_number(&self) -> eyre::Result<u64> {
        Ok(self.provider.last_block_number()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    struct MockProvider {
        snapshots: Arc<AtomicUsize>,
        receipt_hashes: Arc<Mutex<Vec<B256>>>,
    }

    struct MockSnapshot {
        branch: u8,
        receipt_hashes: Arc<Mutex<Vec<B256>>>,
    }

    impl SyncSnapshot for MockSnapshot {
        fn block_by_number(&self, number: u64) -> ProviderResult<Option<HlBlock>> {
            let mut block = HlBlock::default();
            block.header.inner.number = number;
            block.header.inner.extra_data = Bytes::from(vec![self.branch]);
            Ok(Some(block))
        }

        fn receipts_by_block_hash(
            &self,
            hash: B256,
        ) -> ProviderResult<Option<Vec<EthereumReceipt>>> {
            self.receipt_hashes.lock().unwrap().push(hash);
            Ok(Some(vec![]))
        }
    }

    impl ConsistentSyncProvider for MockProvider {
        type Snapshot = MockSnapshot;

        fn consistent_snapshot(&self) -> ProviderResult<Self::Snapshot> {
            let branch = self.snapshots.fetch_add(1, Ordering::SeqCst) as u8;
            Ok(MockSnapshot { branch, receipt_hashes: self.receipt_hashes.clone() })
        }

        fn last_block_number(&self) -> ProviderResult<u64> {
            Ok(0)
        }
    }

    #[test]
    fn batch_uses_one_snapshot_and_hash_bound_receipts() {
        let snapshots = Arc::new(AtomicUsize::new(0));
        let receipt_hashes = Arc::new(Mutex::new(Vec::new()));
        let reader = ProviderSyncReader::new(MockProvider {
            snapshots: snapshots.clone(),
            receipt_hashes: receipt_hashes.clone(),
        });

        let blocks = reader.read_blocks_and_receipts(&[10, 11]).unwrap();

        assert_eq!(snapshots.load(Ordering::SeqCst), 1);
        assert_eq!(blocks.len(), 2);
        assert_eq!(
            *receipt_hashes.lock().unwrap(),
            blocks.iter().map(|b| b.hash()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn separate_requests_may_observe_different_coherent_snapshots() {
        let snapshots = Arc::new(AtomicUsize::new(0));
        let reader = ProviderSyncReader::new(MockProvider {
            snapshots: snapshots.clone(),
            receipt_hashes: Arc::new(Mutex::new(Vec::new())),
        });

        let first = reader.read_blocks_and_receipts(&[10]).unwrap().remove(0);
        let second = reader.read_blocks_and_receipts(&[10]).unwrap().remove(0);

        assert_eq!(snapshots.load(Ordering::SeqCst), 2);
        assert_ne!(first.hash(), second.hash());
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
            .read_blocks_and_receipts(&[height])
            .map_err(|e| internal_rpc_err(format!("Failed to read block {height}: {e}")))?;
        let block = block
            .into_iter()
            .next()
            .ok_or_else(|| internal_rpc_err(format!("No block returned for height {height}")))?;

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

        let blocks = reader
            .read_blocks_and_receipts(heights)
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
