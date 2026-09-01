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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pseudo_peer::{BlockStore, sources::test_utils};
    use std::sync::Arc;

    #[tokio::test]
    async fn refresh_replaces_local_block_and_receipts() {
        let dir = tempfile::tempdir().unwrap();
        let old = test_utils::block(42, 1);
        let new = test_utils::block(42, 2);
        test_utils::write_local(dir.path(), &old);
        let source = Arc::new(Box::new(LocalBlockSource::new(dir.path())) as Box<dyn BlockSource>);
        let store = BlockStore::new(source, None, 998);

        assert_eq!(store.get_by_number(42).await.unwrap().hash(), old.hash());
        test_utils::write_local(dir.path(), &new);
        let (refreshed, changed) = store.refresh_by_number(42).await.unwrap();

        assert!(changed);
        assert_eq!(refreshed.hash(), new.hash());
        assert_eq!(refreshed.highest_precompile_address, new.highest_precompile_address);
        assert!(store.get_by_hash(old.hash()).is_err());
    }

    #[tokio::test]
    async fn refresh_replaces_multi_block_reorg() {
        let dir = tempfile::tempdir().unwrap();
        let ancestor = test_utils::block(39, 9);
        let old_40 = test_utils::block_with_parent(40, 10, ancestor.hash());
        let old_41 = test_utils::block_with_parent(41, 11, old_40.hash());
        let old_42 = test_utils::block_with_parent(42, 12, old_41.hash());
        for block in [&ancestor, &old_40, &old_41, &old_42] {
            test_utils::write_local(dir.path(), block);
        }
        let source = Arc::new(Box::new(LocalBlockSource::new(dir.path())) as Box<dyn BlockSource>);
        let store = BlockStore::new(source, None, 998);
        for height in 39..=42 {
            store.get_by_number(height).await.unwrap();
        }

        let new_40 = test_utils::block_with_parent(40, 20, ancestor.hash());
        let new_41 = test_utils::block_with_parent(41, 21, new_40.hash());
        let new_42 = test_utils::block_with_parent(42, 22, new_41.hash());
        for block in [&new_40, &new_41, &new_42] {
            test_utils::write_local(dir.path(), block);
        }

        let (refreshed, changed) = store.refresh_by_number(40).await.unwrap();
        assert!(changed);
        assert_eq!(refreshed.hash(), new_40.hash());
        assert!(store.get_by_hash(old_40.hash()).is_err());
        assert!(store.get_by_hash(old_41.hash()).is_err());
        assert!(store.get_by_hash(old_42.hash()).is_err());

        let fetched_41 = store.get_by_number(41).await.unwrap();
        let fetched_42 = store.get_by_number(42).await.unwrap();
        assert_eq!(fetched_41.parent_hash(), new_40.hash());
        assert_eq!(fetched_42.parent_hash(), new_41.hash());
        assert_eq!(fetched_42.highest_precompile_address, new_42.highest_precompile_address);
    }
}
