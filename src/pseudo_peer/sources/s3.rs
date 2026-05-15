use super::{BlockSource, utils};
use crate::node::types::BlockAndReceipts;
use aws_sdk_s3::types::RequestPayer;
use futures::{FutureExt, future::BoxFuture};
use reth_metrics::{Metrics, metrics, metrics::Counter};
use std::{sync::Arc, time::Duration};
use tracing::info;

/// Block source that reads blocks from S3 (--s3)
#[derive(Debug, Clone)]
pub struct S3BlockSource {
    client: Arc<aws_sdk_s3::Client>,
    bucket: String,
    polling_interval: Duration,
    metrics: S3BlockSourceMetrics,
}

#[derive(Metrics, Clone)]
#[metrics(scope = "block_source.s3")]
pub struct S3BlockSourceMetrics {
    /// How many times the S3 block source is polling for a block
    pub polling_attempt: Counter,
    /// How many times the S3 block source has polled a block
    pub fetched: Counter,
}

impl S3BlockSource {
    pub fn new(client: aws_sdk_s3::Client, bucket: String, polling_interval: Duration) -> Self {
        Self {
            client: client.into(),
            bucket,
            polling_interval,
            metrics: S3BlockSourceMetrics::default(),
        }
    }

    async fn pick_path_with_highest_number(
        client: &aws_sdk_s3::Client,
        bucket: &str,
        dir: &str,
        is_dir: bool,
    ) -> eyre::Result<Option<(u64, String)>> {
        let request = client
            .list_objects()
            .bucket(bucket)
            .prefix(dir)
            .delimiter("/")
            .request_payer(RequestPayer::Requester);
        let response = request.send().await?;
        let files: Vec<String> = if is_dir {
            response
                .common_prefixes
                .unwrap_or_default()
                .iter()
                .filter_map(|object| object.prefix.as_ref().map(ToString::to_string))
                .collect()
        } else {
            response
                .contents
                .unwrap_or_default()
                .iter()
                .filter_map(|object| object.key.as_ref().map(ToString::to_string))
                .collect()
        };
        Ok(utils::name_with_largest_number(&files, is_dir))
    }
}

impl BlockSource for S3BlockSource {
    fn collect_block(&self, height: u64) -> BoxFuture<'static, eyre::Result<BlockAndReceipts>> {
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let metrics = self.metrics.clone();
        async move {
            let path = utils::rmp_path(height);
            metrics.polling_attempt.increment(1);

            let request = client
                .get_object()
                .request_payer(RequestPayer::Requester)
                .bucket(&bucket)
                .key(path);
            let response = request.send().await?;
            metrics.fetched.increment(1);
            let bytes = response.body.collect().await?.into_bytes();
            let mut decoder = lz4_flex::frame::FrameDecoder::new(&bytes[..]);
            let blocks: Vec<BlockAndReceipts> = rmp_serde::from_read(&mut decoder)?;
            Ok(blocks[0].clone())
        }
        .boxed()
    }

    fn find_latest_block_number(&self) -> BoxFuture<'static, eyre::Result<Option<u64>>> {
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        async move {
            let Some((_, first_level)) =
                Self::pick_path_with_highest_number(&client, &bucket, "", true).await?
            else {
                return Ok(None);
            };
            let Some((_, second_level)) =
                Self::pick_path_with_highest_number(&client, &bucket, &first_level, true).await?
            else {
                return Ok(None);
            };
            let Some((block_number, third_level)) =
                Self::pick_path_with_highest_number(&client, &bucket, &second_level, false).await?
            else {
                return Ok(None);
            };

            info!("Latest block number: {} with path {}", block_number, third_level);
            Ok(Some(block_number))
        }
        .boxed()
    }

    /// Uses `tokio::spawn` batches instead of the default `buffered()` stream.
    /// This distributes response processing across the runtime's thread pool,
    /// avoiding the single-task polling bottleneck (~10x faster on fast networks).
    fn collect_blocks(
        &self,
        heights: Vec<u64>,
    ) -> BoxFuture<'static, eyre::Result<Vec<BlockAndReceipts>>> {
        let concurrency = self.recommended_chunk_size() as usize;
        let futs: Vec<_> = heights
            .into_iter()
            .map(|h| self.collect_block(h))
            .collect();
        async move {
            let mut results = Vec::with_capacity(futs.len());
            let mut futs = futs.into_iter();
            loop {
                let batch: Vec<_> = (&mut futs)
                    .take(concurrency)
                    .map(|fut| tokio::spawn(fut))
                    .collect();
                if batch.is_empty() {
                    break;
                }
                for handle in batch {
                    results.push(handle.await.map_err(|e| eyre::eyre!(e))??);
                }
            }
            Ok(results)
        }
        .boxed()
    }

    fn recommended_chunk_size(&self) -> u64 {
        1000
    }

    fn polling_interval(&self) -> Duration {
        self.polling_interval
    }
}
