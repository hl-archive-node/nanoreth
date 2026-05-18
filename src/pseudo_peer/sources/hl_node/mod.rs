mod cache;
mod file_ops;
mod scan;
#[cfg(test)]
mod tests;
mod time_utils;

use self::{
    cache::LocalBlocksCache,
    file_ops::FileOperations,
    scan::{LineStream, ScanOptions, Scanner},
    time_utils::TimeUtils,
};
use super::{BlockSource, BlockSourceBoxed};
use crate::node::types::BlockAndReceipts;
use futures::future::BoxFuture;
use reth_metrics::{Metrics, metrics, metrics::Counter};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use time::OffsetDateTime;
use tokio::sync::Mutex;
use tracing::{info, warn};

const HOURLY_SUBDIR: &str = "hourly";
const CACHE_SIZE: u32 = 8000; // 3660 blocks per hour
const ONE_HOUR: Duration = Duration::from_secs(60 * 60);
const TAIL_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Debug, Clone)]
pub struct HlNodeBlockSourceArgs {
    pub root: PathBuf,
    pub fallback_threshold: Duration,
}

/// Block source that monitors the local ingest directory for the HL node.
#[derive(Debug, Clone)]
pub struct HlNodeBlockSource {
    pub fallback: BlockSourceBoxed,
    pub local_blocks_cache: Arc<Mutex<LocalBlocksCache>>,
    pub last_local_fetch: Arc<Mutex<Option<(u64, OffsetDateTime)>>>,
    pub args: HlNodeBlockSourceArgs,
    pub metrics: HlNodeBlockSourceMetrics,
}

#[derive(Metrics, Clone)]
#[metrics(scope = "block_source.hl_node")]
pub struct HlNodeBlockSourceMetrics {
    /// How many times the HL node block source is polling for a block
    pub fetched_from_hl_node: Counter,
    /// How many times the HL node block source is fetched from the fallback
    pub fetched_from_fallback: Counter,
    /// How many times `try_collect_local_block` was faster than ingest loop
    pub file_read_triggered: Counter,
}

impl BlockSource for HlNodeBlockSource {
    fn collect_block(&self, height: u64) -> BoxFuture<'static, eyre::Result<BlockAndReceipts>> {
        let fallback = self.fallback.clone();
        let args = self.args.clone();
        let local_blocks_cache = self.local_blocks_cache.clone();
        let last_local_fetch = self.last_local_fetch.clone();
        let metrics = self.metrics.clone();
        Box::pin(async move {
            let now = OffsetDateTime::now_utc();

            if let Some(block) =
                Self::try_collect_local_block(&metrics, local_blocks_cache, height).await
            {
                Self::update_last_fetch(last_local_fetch, height, now).await;
                metrics.fetched_from_hl_node.increment(1);
                return Ok(block);
            }

            if let Some((last_height, last_poll_time)) = *last_local_fetch.lock().await {
                let more_recent = last_height < height;
                let too_soon = now - last_poll_time < args.fallback_threshold;
                if more_recent && too_soon {
                    return Err(eyre::eyre!(
                        "Not found locally; limiting polling rate before fallback so that hl-node has chance to catch up"
                    ));
                }
            }

            let block = fallback.collect_block(height).await?;
            metrics.fetched_from_fallback.increment(1);
            Self::update_last_fetch(last_local_fetch, height, now).await;
            Ok(block)
        })
    }

    fn find_latest_block_number(&self) -> BoxFuture<'static, Option<u64>> {
        let fallback = self.fallback.clone();
        let args = self.args.clone();
        Box::pin(async move {
            let Some(dir) = FileOperations::find_latest_hourly_file(&args.root) else {
                warn!(
                    "No EVM blocks from hl-node found at {:?}; fallback to s3/ingest-dir",
                    args.root
                );
                return fallback.find_latest_block_number().await;
            };

            match FileOperations::read_last_block_from_file(&dir) {
                Some((_, height)) => {
                    info!("Latest block number: {} with path {}", height, dir.display());
                    Some(height)
                }
                None => {
                    warn!(
                        "Failed to parse the hl-node hourly file at {:?}; fallback to s3/ingest-dir",
                        dir
                    );
                    fallback.find_latest_block_number().await
                }
            }
        })
    }

    fn recommended_chunk_size(&self) -> u64 {
        self.fallback.recommended_chunk_size()
    }
}

struct CurrentFile {
    path: PathBuf,
    line_stream: Option<LineStream>,
}

impl CurrentFile {
    fn from_datetime(dt: OffsetDateTime, root: &Path) -> Self {
        let (hour, day_str) = (dt.hour(), TimeUtils::date_from_datetime(dt));
        let path = root.join(HOURLY_SUBDIR).join(&day_str).join(format!("{}", hour));
        Self { path, line_stream: None }
    }

    fn open(&mut self) -> eyre::Result<()> {
        if self.line_stream.is_some() {
            return Ok(());
        }

        self.line_stream = Some(LineStream::from_path(&self.path)?);
        Ok(())
    }
}

/// Checks if a file has any blocks (i.e., hl-node is actively writing to it).
fn file_has_blocks(path: &Path) -> bool {
    LineStream::from_path(path).is_ok_and(|mut stream| {
        !Scanner::scan_hour_file(
            &mut stream,
            ScanOptions { start_height: 0, only_load_ranges: true },
        )
        .new_block_ranges
        .is_empty()
    })
}

impl HlNodeBlockSource {
    async fn update_last_fetch(
        last_local_fetch: Arc<Mutex<Option<(u64, OffsetDateTime)>>>,
        height: u64,
        now: OffsetDateTime,
    ) {
        let mut last_fetch = last_local_fetch.lock().await;
        if last_fetch.is_none_or(|(h, _)| h < height) {
            *last_fetch = Some((height, now));
        }
    }

    async fn try_collect_local_block(
        metrics: &HlNodeBlockSourceMetrics,
        local_blocks_cache: Arc<Mutex<LocalBlocksCache>>,
        height: u64,
    ) -> Option<BlockAndReceipts> {
        let mut u_cache = local_blocks_cache.lock().await;
        if let Some(block) = u_cache.get_block(height) {
            return Some(block);
        }
        let path = u_cache.get_path_for_height(height)?;
        info!("Loading block data from {:?}", path);
        metrics.file_read_triggered.increment(1);
        let mut line_stream = LineStream::from_path(&path).ok()?;
        let scan_result = Scanner::scan_hour_file(
            &mut line_stream,
            ScanOptions { start_height: 0, only_load_ranges: false },
        );
        u_cache.load_scan_result(scan_result);
        u_cache.get_block(height)
    }

    async fn try_backfill_local_blocks(
        root: &Path,
        cache: &Arc<Mutex<LocalBlocksCache>>,
        cutoff_height: u64,
    ) -> eyre::Result<()> {
        let mut u_cache = cache.lock().await;
        for subfile in FileOperations::all_hourly_files(root).unwrap_or_default() {
            if let Some((_, height)) = FileOperations::read_last_block_from_file(&subfile) {
                if height < cutoff_height {
                    continue;
                }
            } else {
                warn!("Failed to parse last line of file: {:?}", subfile);
            }
            let mut line_stream =
                LineStream::from_path(&subfile).expect("Failed to open line stream");
            let mut scan_result = Scanner::scan_hour_file(
                &mut line_stream,
                ScanOptions { start_height: cutoff_height, only_load_ranges: true },
            );
            scan_result.new_blocks.clear(); // Only store ranges, load data lazily
            u_cache.load_scan_result(scan_result);
        }
        u_cache.log_range_summary(root);
        Ok(())
    }

    async fn start_local_ingest_loop(&self, current_head: u64) {
        let root = self.args.root.to_owned();
        let cache = self.local_blocks_cache.clone();
        tokio::spawn(async move {
            let mut next_height = current_head;
            let mut dt = loop {
                if let Some(f) = FileOperations::find_latest_hourly_file(&root) {
                    break TimeUtils::datetime_from_path(&f).unwrap();
                }
                tokio::time::sleep(TAIL_INTERVAL).await;
            };
            let mut current_file = CurrentFile::from_datetime(dt, &root);
            info!("Starting local ingest loop from height: {}", current_head);
            loop {
                let _ = current_file.open();
                if let Some(line_stream) = &mut current_file.line_stream {
                    let scan_result = Scanner::scan_hour_file(
                        line_stream,
                        ScanOptions { start_height: next_height, only_load_ranges: false },
                    );
                    next_height = scan_result.next_expected_height;
                    cache.lock().await.load_scan_result(scan_result);
                }
                // Check if we should switch to the next hourly file
                let now = OffsetDateTime::now_utc();
                let next_dt = dt + ONE_HOUR;
                if next_dt < now {
                    let next_file = CurrentFile::from_datetime(next_dt, &root);
                    if file_has_blocks(&next_file.path) {
                        // Final scan of current file to catch any late-written blocks
                        if let Some(line_stream) = &mut current_file.line_stream {
                            let scan_result = Scanner::scan_hour_file(
                                line_stream,
                                ScanOptions { start_height: next_height, only_load_ranges: false },
                            );
                            next_height = scan_result.next_expected_height;
                            cache.lock().await.load_scan_result(scan_result);
                        }
                        dt = next_dt;
                        current_file = next_file;
                        info!("Moving to new file: {:?}", current_file.path);
                        continue; // Start reading new file immediately
                    }
                }
                tokio::time::sleep(TAIL_INTERVAL).await;
            }
        });
    }

    pub(crate) async fn run(&self, next_block_number: u64) -> eyre::Result<()> {
        let _ = Self::try_backfill_local_blocks(
            &self.args.root,
            &self.local_blocks_cache,
            next_block_number,
        )
        .await;
        self.start_local_ingest_loop(next_block_number).await;
        Ok(())
    }

    pub async fn new(
        fallback: BlockSourceBoxed,
        args: HlNodeBlockSourceArgs,
        next_block_number: u64,
    ) -> Self {
        let block_source = Self {
            fallback,
            args,
            local_blocks_cache: Arc::new(Mutex::new(LocalBlocksCache::new(CACHE_SIZE))),
            last_local_fetch: Arc::new(Mutex::new(None)),
            metrics: HlNodeBlockSourceMetrics::default(),
        };
        block_source.run(next_block_number).await.unwrap();
        block_source
    }
}
