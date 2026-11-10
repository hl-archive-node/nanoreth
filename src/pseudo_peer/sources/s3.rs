use super::{BlockSource, utils};
use crate::node::types::BlockAndReceipts;
use aws_sdk_s3::types::RequestPayer;
use futures::{FutureExt, future::BoxFuture};
use reth_metrics::{Metrics, metrics, metrics::Counter};
use std::{sync::Arc, time::Duration};
use tracing::{info, warn};

/// Attempts per block in a range fetch before the whole range fails.
const RANGE_FETCH_MAX_ATTEMPTS: u32 = 6;
/// First retry delay in a range fetch; doubles each attempt (0.5 + 1 + 2 + 4 + 8 s).
const RANGE_FETCH_INITIAL_BACKOFF: Duration = Duration::from_millis(500);

/// Block source that reads blocks from S3 (--s3)
#[derive(Debug, Clone)]
pub struct S3BlockSource {
    client: Arc<aws_sdk_s3::Client>,
    bucket: String,
    polling_interval: Duration,
    concurrency: usize,
    metrics: S3BlockSourceMetrics,
}

#[derive(Metrics, Clone)]
#[metrics(scope = "block_source.s3")]
pub struct S3BlockSourceMetrics {
    /// How many times the S3 block source is polling for a block
    pub polling_attempt: Counter,
    /// How many times the S3 block source has polled a block
    pub fetched: Counter,
    /// How many range block fetches were retried after a failure
    pub retried: Counter,
}

impl S3BlockSource {
    pub fn new(
        client: aws_sdk_s3::Client,
        bucket: String,
        polling_interval: Duration,
        concurrency: usize,
    ) -> Self {
        Self {
            client: client.into(),
            bucket,
            polling_interval,
            concurrency,
            metrics: S3BlockSourceMetrics::default(),
        }
    }

    /// Fetches a block inside a requested range, retrying transient failures such as stalled
    /// streams or dispatch failures.
    async fn collect_block_with_retry(self, height: u64) -> eyre::Result<BlockAndReceipts> {
        utils::retry_with_backoff(
            RANGE_FETCH_MAX_ATTEMPTS,
            RANGE_FETCH_INITIAL_BACKOFF,
            || self.collect_block(height),
            |attempt, err| {
                self.metrics.retried.increment(1);
                warn!(height, attempt, %err, "S3 block fetch failed, retrying");
            },
        )
        .await
    }

    async fn pick_path_with_highest_number(
        client: &aws_sdk_s3::Client,
        bucket: &str,
        dir: &str,
        is_dir: bool,
    ) -> Option<(u64, String)> {
        let request = client
            .list_objects()
            .bucket(bucket)
            .prefix(dir)
            .delimiter("/")
            .request_payer(RequestPayer::Requester);
        let response = request.send().await.ok()?;
        let files: Vec<String> = if is_dir {
            response
                .common_prefixes?
                .iter()
                .map(|object| object.prefix.as_ref().unwrap().to_string())
                .collect()
        } else {
            response
                .contents?
                .iter()
                .map(|object| object.key.as_ref().unwrap().to_string())
                .collect()
        };
        utils::name_with_largest_number(&files, is_dir)
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

    fn find_latest_block_number(&self) -> BoxFuture<'static, Option<u64>> {
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        async move {
            let (_, first_level) =
                Self::pick_path_with_highest_number(&client, &bucket, "", true).await?;
            let (_, second_level) =
                Self::pick_path_with_highest_number(&client, &bucket, &first_level, true).await?;
            let (block_number, third_level) =
                Self::pick_path_with_highest_number(&client, &bucket, &second_level, false).await?;

            info!("Latest block number: {} with path {}", block_number, third_level);
            Some(block_number)
        }
        .boxed()
    }

    /// Uses `tokio::spawn` instead of the default `buffered()` stream. This distributes response
    /// processing across the runtime's thread pool, avoiding the single-task polling bottleneck
    /// (~10x faster on fast networks).
    ///
    /// Each block is retried on its own: otherwise one stalled GET (common on a congested link)
    /// fails the whole range, the peer re-requests it from scratch and the range never completes.
    fn collect_blocks(
        &self,
        heights: Vec<u64>,
    ) -> BoxFuture<'static, eyre::Result<Vec<BlockAndReceipts>>> {
        let source = self.clone();
        async move {
            let concurrency = source.concurrency;
            utils::fetch_ordered(heights, concurrency, move |height| {
                source.clone().collect_block_with_retry(height)
            })
            .await
        }
        .boxed()
    }

    fn recommended_chunk_size(&self) -> u64 {
        self.concurrency as u64
    }

    fn polling_interval(&self) -> Duration {
        self.polling_interval
    }
}
