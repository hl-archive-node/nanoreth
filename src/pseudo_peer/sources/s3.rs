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

    /// Uses `tokio::spawn` batches instead of the default `buffered()` stream.
    /// This distributes response processing across the runtime's thread pool,
    /// avoiding the single-task polling bottleneck (~10x faster on fast networks).
    fn collect_blocks(
        &self,
        heights: Vec<u64>,
    ) -> BoxFuture<'static, eyre::Result<Vec<BlockAndReceipts>>> {
        let concurrency = self.recommended_chunk_size() as usize;
        let futs: Vec<_> = heights.into_iter().map(|h| self.collect_block(h)).collect();
        async move {
            let mut results = Vec::with_capacity(futs.len());
            let mut futs = futs.into_iter();
            loop {
                let batch: Vec<_> =
                    (&mut futs).take(concurrency).map(|fut| tokio::spawn(fut)).collect();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pseudo_peer::{BlockStore, sources::test_utils};
    use aws_sdk_s3::config::{Credentials, Region};
    use parking_lot::RwLock;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[tokio::test]
    async fn refresh_replaces_s3_block_and_receipts() {
        let body = Arc::new(RwLock::new(test_utils::encode(&[test_utils::block(42, 1)])));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let served_body = body.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let body = served_body.clone();
                tokio::spawn(async move {
                    let mut request = vec![0; 8192];
                    let _ = stream.read(&mut request).await.unwrap();
                    let payload = body.read().clone();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                    stream.write_all(&payload).await.unwrap();
                });
            }
        });
        let config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .region(Region::new("us-east-1"))
            .credentials_provider(Credentials::new("test", "test", None, None, "test"))
            .endpoint_url(format!("http://{address}"))
            .force_path_style(true)
            .build();
        let source = Arc::new(Box::new(S3BlockSource::new(
            aws_sdk_s3::Client::from_conf(config),
            "blocks".into(),
            Duration::from_millis(1),
        )) as Box<dyn BlockSource>);
        let store = BlockStore::new(source, None, 998);

        let old_hash = store.get_by_number(42).await.unwrap().hash();
        *body.write() = test_utils::encode(&[test_utils::block(42, 2)]);
        let (refreshed, changed) = store.refresh_by_number(42).await.unwrap();

        assert!(changed);
        assert_ne!(refreshed.hash(), old_hash);
        assert!(store.get_by_hash(old_hash).await.is_err());
        server.abort();
    }
}
