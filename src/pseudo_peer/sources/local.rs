use super::{BlockSource, utils};
use crate::node::types::BlockAndReceipts;
use eyre::Context;
use futures::{FutureExt, future::BoxFuture};
use reth_metrics::{Metrics, metrics, metrics::Counter};
use std::path::PathBuf;
use tracing::info;

/// Block source that reads blocks from local filesystem (--ingest-dir)
#[derive(Debug, Clone)]
pub struct LocalBlockSource {
    dir: PathBuf,
    metrics: LocalBlockSourceMetrics,
}

#[derive(Metrics, Clone)]
#[metrics(scope = "block_source.local")]
pub struct LocalBlockSourceMetrics {
    /// How many times the local block source is polling for a block
    pub polling_attempt: Counter,
    /// How many times the local block source is fetched from the local filesystem
    pub fetched: Counter,
}

impl LocalBlockSource {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), metrics: LocalBlockSourceMetrics::default() }
    }

    async fn pick_path_with_highest_number(dir: PathBuf, is_dir: bool) -> Option<(u64, String)> {
        let files = std::fs::read_dir(&dir).unwrap().collect::<Vec<_>>();
        let files = files
            .into_iter()
            .filter(|path| path.as_ref().unwrap().path().is_dir() == is_dir)
            .map(|entry| entry.unwrap().path().to_string_lossy().to_string())
            .collect::<Vec<_>>();

        utils::name_with_largest_number(&files, is_dir)
    }
}

impl BlockSource for LocalBlockSource {
    fn collect_block(&self, height: u64) -> BoxFuture<'static, eyre::Result<BlockAndReceipts>> {
        let dir = self.dir.clone();
        let metrics = self.metrics.clone();
        async move {
            let path = dir.join(utils::rmp_path(height));
            metrics.polling_attempt.increment(1);

            let file = tokio::fs::read(&path)
                .await
                .wrap_err_with(|| format!("Failed to read block from {path:?}"))?;
            let mut decoder = lz4_flex::frame::FrameDecoder::new(&file[..]);
            let blocks: Vec<BlockAndReceipts> = rmp_serde::from_read(&mut decoder)?;
            metrics.fetched.increment(1);
            Ok(blocks[0].clone())
        }
        .boxed()
    }

    fn find_latest_block_number(&self) -> BoxFuture<'static, Option<u64>> {
        let dir = self.dir.clone();
        async move {
            let (_, first_level) = Self::pick_path_with_highest_number(dir.clone(), true).await?;
            let (_, second_level) =
                Self::pick_path_with_highest_number(dir.join(first_level), true).await?;
            let (block_number, third_level) =
                Self::pick_path_with_highest_number(dir.join(second_level), false).await?;

            info!("Latest block number: {} with path {}", block_number, third_level);
            Some(block_number)
        }
        .boxed()
    }

    fn recommended_chunk_size(&self) -> u64 {
        1000
    }
}
