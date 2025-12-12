use crate::node::types::BlockAndReceipts;
use auto_impl::auto_impl;
use futures::future::BoxFuture;
use std::{sync::Arc, time::Duration};

// Module declarations
mod cached;
mod hl_node;
mod local;
mod rpc;
mod s3;
mod utils;

// Public exports
pub use cached::CachedBlockSource;
pub use hl_node::{HlNodeBlockSource, HlNodeBlockSourceArgs};
pub use local::LocalBlockSource;
pub use rpc::RpcBlockSource;
pub use s3::S3BlockSource;

const DEFAULT_POLLING_INTERVAL: Duration = Duration::from_millis(25);

/// Trait for block sources that can retrieve blocks from various sources
#[auto_impl(&, &mut, Box, Arc)]
pub trait BlockSource: Send + Sync + std::fmt::Debug + Unpin + 'static {
    /// Retrieves a block at the specified height
    fn collect_block(&self, height: u64) -> BoxFuture<'static, eyre::Result<BlockAndReceipts>>;

    /// Finds the latest block number available from this source
    fn find_latest_block_number(&self) -> BoxFuture<'static, Option<u64>>;

    /// Returns the recommended chunk size for batch operations
    fn recommended_chunk_size(&self) -> u64;

    /// Returns the polling interval
    fn polling_interval(&self) -> Duration {
        DEFAULT_POLLING_INTERVAL
    }
}

/// Type alias for a boxed block source
pub type BlockSourceBoxed = Arc<Box<dyn BlockSource>>;
