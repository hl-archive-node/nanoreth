use super::{utils, BlockSource};
use crate::node::types::BlockAndReceipts;
use aws_sdk_s3::types::RequestPayer;
use futures::{future::BoxFuture, FutureExt};
use reth_metrics::{metrics, metrics::Counter, Metrics};
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

    fn recommended_chunk_size(&self) -> u64 {
        1000
    }

    fn polling_interval(&self) -> Duration {
        self.polling_interval
    }
}
