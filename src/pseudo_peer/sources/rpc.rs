use super::BlockSource;
use crate::node::types::BlockAndReceipts;
use alloy_primitives::Bytes;
use futures::{FutureExt, StreamExt, future::BoxFuture};
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use jsonrpsee_core::client::ClientT;
use reth_metrics::{Metrics, metrics, metrics::Counter};
use std::{sync::Arc, time::Duration};
use tracing::info;

/// Block source that fetches blocks from a remote nanoreth node via RPC.
///
/// Connects to another nanoreth node running with `--enable-sync-server`
/// and fetches blocks through the `hl_sync` RPC namespace.
#[derive(Debug, Clone)]
pub struct RpcBlockSource {
    client: Arc<HttpClient>,
    polling_interval: Duration,
    metrics: RpcBlockSourceMetrics,
}

#[derive(Metrics, Clone)]
#[metrics(scope = "block_source.rpc")]
pub struct RpcBlockSourceMetrics {
    /// How many times the RPC block source is polling for a block
    pub polling_attempt: Counter,
    /// How many times the RPC block source has fetched a block
    pub fetched: Counter,
}

impl RpcBlockSource {
    pub fn new(url: String, polling_interval: Duration) -> Self {
        let client = HttpClientBuilder::default()
            .request_timeout(Duration::from_secs(120))
            .build(&url)
            .unwrap_or_else(|e| panic!("Failed to build RPC client for {url}: {e}"));
        info!("RPC block source connected to {url}");
        Self {
            client: Arc::new(client),
            polling_interval,
            metrics: RpcBlockSourceMetrics::default(),
        }
    }
}

impl BlockSource for RpcBlockSource {
    fn collect_block(&self, height: u64) -> BoxFuture<'static, eyre::Result<BlockAndReceipts>> {
        let client = self.client.clone();
        let metrics = self.metrics.clone();
        async move {
            metrics.polling_attempt.increment(1);
            let bytes: Bytes = client.request("hl_syncGetBlock", (height,)).await?;
            let mut decoder = lz4_flex::frame::FrameDecoder::new(&bytes[..]);
            let blocks: Vec<BlockAndReceipts> = rmp_serde::from_read(&mut decoder)?;
            metrics.fetched.increment(1);
            Ok(blocks[0].clone())
        }
        .boxed()
    }

    fn find_latest_block_number(&self) -> BoxFuture<'static, Option<u64>> {
        let client = self.client.clone();
        async move {
            let result: Option<u64> =
                client.request("hl_syncLatestBlockNumber", Vec::<u64>::new()).await.ok()?;
            info!("Latest block number from remote: {:?}", result);
            result
        }
        .boxed()
    }

    fn collect_blocks(
        &self,
        heights: Vec<u64>,
    ) -> BoxFuture<'static, eyre::Result<Vec<BlockAndReceipts>>> {
        let client = self.client.clone();
        let metrics = self.metrics.clone();
        async move {
            const BATCH_SIZE: usize = 500;
            const MAX_CONCURRENT_BATCHES: usize = 20;

            let batches: Vec<Vec<u64>> = heights.chunks(BATCH_SIZE).map(|c| c.to_vec()).collect();

            let results: Vec<eyre::Result<Vec<BlockAndReceipts>>> = futures::stream::iter(batches)
                .map(|batch| {
                    let client = client.clone();
                    let metrics = metrics.clone();
                    async move {
                        metrics.polling_attempt.increment(batch.len() as u64);
                        let bytes: Bytes = client.request("hl_syncGetBlocks", (batch,)).await?;
                        let mut decoder = lz4_flex::frame::FrameDecoder::new(&bytes[..]);
                        let blocks: Vec<BlockAndReceipts> = rmp_serde::from_read(&mut decoder)?;
                        metrics.fetched.increment(blocks.len() as u64);
                        Ok(blocks)
                    }
                })
                .buffered(MAX_CONCURRENT_BATCHES)
                .collect()
                .await;

            let mut all_blocks = Vec::with_capacity(heights.len());
            for result in results {
                all_blocks.extend(result?);
            }
            Ok(all_blocks)
        }
        .boxed()
    }

    fn recommended_chunk_size(&self) -> u64 {
        200
    }

    fn polling_interval(&self) -> Duration {
        self.polling_interval
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pseudo_peer::{BlockStore, sources::test_utils};
    use jsonrpsee::{RpcModule, server::ServerBuilder};
    use parking_lot::RwLock;

    #[tokio::test]
    async fn refresh_replaces_rpc_block_and_receipts() {
        let current = Arc::new(RwLock::new(test_utils::block(42, 1)));
        let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        let mut module = RpcModule::new(current.clone());
        module
            .register_method("hl_syncGetBlock", |_, current, _| {
                Ok::<_, jsonrpsee_types::ErrorObjectOwned>(Bytes::from(test_utils::encode(
                    std::slice::from_ref(&*current.read()),
                )))
            })
            .unwrap();
        let handle = server.start(module);
        let source = Arc::new(Box::new(RpcBlockSource::new(
            format!("http://{address}"),
            Duration::from_millis(1),
        )) as Box<dyn BlockSource>);
        let store = BlockStore::new(source, None, 998);

        let old_hash = store.get_by_number(42).await.unwrap().hash();
        *current.write() = test_utils::block(42, 2);
        let (refreshed, changed) = store.refresh_by_number(42).await.unwrap();

        assert!(changed);
        assert_ne!(refreshed.hash(), old_hash);
        assert!(store.get_by_hash(old_hash).await.is_err());
        handle.stop().unwrap();
    }
}
