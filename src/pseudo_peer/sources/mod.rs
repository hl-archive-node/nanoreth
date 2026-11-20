use crate::node::types::BlockAndReceipts;
use auto_impl::auto_impl;
use futures::{FutureExt, StreamExt, future::BoxFuture};
use std::{sync::Arc, time::Duration};

// Module declarations
mod hl_node;
mod local;
mod rpc;
mod s3;
#[cfg(test)]
pub(super) mod test_utils;
mod utils;

// Public exports
pub use hl_node::{HlNodeBlockSource, HlNodeBlockSourceArgs};
pub use local::LocalBlockSource;
pub use rpc::RpcBlockSource;
pub use s3::S3BlockSource;

const DEFAULT_POLLING_INTERVAL: Duration = Duration::from_millis(25);

/// Whether a [`BlockSource::collect_block`] error means the source does not have the block (a
/// missing S3 object or local file), as opposed to a failure worth retrying.
pub fn is_block_not_found(err: &eyre::Report) -> bool {
    use aws_sdk_s3::{error::SdkError, operation::get_object::GetObjectError};
    err.chain().any(|err| {
        if let Some(err) = err.downcast_ref::<std::io::Error>() {
            return err.kind() == std::io::ErrorKind::NotFound;
        }
        if let Some(err) = err.downcast_ref::<SdkError<GetObjectError>>() {
            return err.as_service_error().is_some_and(GetObjectError::is_no_such_key) ||
                err.raw_response().is_some_and(|response| response.status().as_u16() == 404);
        }
        false
    })
}

/// Trait for block sources that can retrieve blocks from various sources
#[auto_impl(&, &mut, Box, Arc)]
pub trait BlockSource: Send + Sync + std::fmt::Debug + Unpin + 'static {
    /// Retrieves a block at the specified height
    fn collect_block(&self, height: u64) -> BoxFuture<'static, eyre::Result<BlockAndReceipts>>;

    /// Retrieves a block without trusting any source-local content cache.
    fn refresh_block(&self, height: u64) -> BoxFuture<'static, eyre::Result<BlockAndReceipts>> {
        self.collect_block(height)
    }

    /// Finds the latest block number available from this source
    fn find_latest_block_number(&self) -> BoxFuture<'static, Option<u64>>;

    /// Returns the recommended chunk size for batch operations
    fn recommended_chunk_size(&self) -> u64;

    /// Retrieves multiple blocks by height. Default implementation uses
    /// buffered concurrent calls to `collect_block`. Sources like RPC
    /// can override this to use batch endpoints for better performance.
    fn collect_blocks(
        &self,
        heights: Vec<u64>,
    ) -> BoxFuture<'static, eyre::Result<Vec<BlockAndReceipts>>> {
        let chunk_size = self.recommended_chunk_size() as usize;
        let futs: Vec<_> = heights.into_iter().map(|h| self.collect_block(h)).collect();
        async move {
            futures::stream::iter(futs)
                .buffered(chunk_size)
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect()
        }
        .boxed()
    }

    /// Returns the polling interval
    fn polling_interval(&self) -> Duration {
        DEFAULT_POLLING_INTERVAL
    }
}

/// Type alias for a boxed block source
pub type BlockSourceBoxed = Arc<Box<dyn BlockSource>>;

#[cfg(test)]
mod not_found_tests {
    use super::is_block_not_found;
    use eyre::WrapErr;

    #[test]
    fn missing_file_is_not_found_but_other_errors_are_not() {
        let missing: eyre::Result<()> =
            Err(std::io::Error::from(std::io::ErrorKind::NotFound)).wrap_err("Failed to read");
        assert!(is_block_not_found(&missing.unwrap_err()));
        let denied: eyre::Result<()> = Err(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        ))
        .wrap_err("Failed to read");
        assert!(!is_block_not_found(&denied.unwrap_err()));
        assert!(!is_block_not_found(&eyre::eyre!("timeout")));
    }
}
