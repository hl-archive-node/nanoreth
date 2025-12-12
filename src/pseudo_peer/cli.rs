use std::time::Duration;

use crate::pseudo_peer::HlNodeBlockSourceArgs;

use super::config::BlockSourceConfig;
use clap::{Args, Parser};
use reth_node_core::args::LogArgs;

#[derive(Debug, Clone, Args)]
pub struct BlockSourceArgs {
    /// Block source to use for the benchmark.
    /// Example: s3://hl-mainnet-evm-blocks
    /// Example: /home/user/personal/evm-blocks
    ///
    /// For S3, you can use environment variables like AWS_PROFILE, etc.
    #[arg(long, alias = "ingest-dir")]
    block_source: Option<String>,

    #[arg(long, alias = "local-ingest-dir")]
    local_ingest_dir: Option<String>,

    /// Shorthand of --block-source=s3://hl-mainnet-evm-blocks
    #[arg(long, default_value_t = false)]
    s3: bool,

    /// Shorthand of --block-source-from-node=~/hl/data/evm_blocks_and_receipts
    #[arg(long)]
    local: bool,

    /// Fetch blocks from trusted nanoreth RPC node(s).
    /// Supports comma-separated URLs for multi-peer round-robin with failover.
    ///
    /// Examples:
    ///   --block-source-rpc=http://peer1:8545
    ///   --block-source-rpc=http://peer1:8545,http://peer2:8545,http://peer3:8545
    ///
    /// This uses batched RPC calls (debug_getRawBlock, debug_getRawReceipts,
    /// eth_blockPrecompileData) to fetch blocks from another nanoreth node.
    #[arg(long, env = "BLOCK_SOURCE_RPC")]
    block_source_rpc: Option<String>,

    /// Interval for polling new blocks from RPC in milliseconds.
    #[arg(id = "rpc.polling-interval", long = "rpc.polling-interval", default_value = "100")]
    rpc_polling_interval: u64,

    /// Interval for polling new blocks in S3 in milliseconds.
    #[arg(id = "s3.polling-interval", long = "s3.polling-interval", default_value = "25")]
    s3_polling_interval: u64,

    /// Maximum allowed delay for the hl-node block source in milliseconds.
    /// If this threshold is exceeded, the client falls back to other sources.
    #[arg(
        id = "local.fallback-threshold",
        long = "local.fallback-threshold",
        default_value = "5000"
    )]
    local_fallback_threshold: u64,
}

impl BlockSourceArgs {
    pub async fn parse(&self) -> eyre::Result<BlockSourceConfig> {
        let config = self.create_base_config().await?;
        let config = self.apply_node_source_config(config);
        Ok(config)
    }

    async fn create_base_config(&self) -> eyre::Result<BlockSourceConfig> {
        if self.s3 {
            return Ok(BlockSourceConfig::s3_default(Duration::from_millis(
                self.s3_polling_interval,
            ))
            .await);
        }

        if self.local {
            return Ok(BlockSourceConfig::local_default());
        }

        // Check for RPC block source (supports comma-separated URLs)
        if let Some(rpc_urls_str) = self.block_source_rpc.as_ref() {
            let urls: Vec<String> = rpc_urls_str
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            return Ok(BlockSourceConfig::rpc(
                urls,
                Duration::from_millis(self.rpc_polling_interval),
            ));
        }

        let Some(value) = self.block_source.as_ref() else {
            return Err(eyre::eyre!(
                "You need to specify a block source e.g., --s3 or --block-source=/path/to/blocks or --block-source-rpc=http://..."
            ));
        };

        if let Some(bucket) = value.strip_prefix("s3://") {
            Ok(BlockSourceConfig::s3(
                bucket.to_string(),
                Duration::from_millis(self.s3_polling_interval),
            )
            .await)
        } else {
            Ok(BlockSourceConfig::local(value.into()))
        }
    }

    fn apply_node_source_config(&self, config: BlockSourceConfig) -> BlockSourceConfig {
        let Some(local_ingest_dir) = self.local_ingest_dir.as_ref() else {
            return config;
        };

        config.with_block_source_from_node(HlNodeBlockSourceArgs {
            root: local_ingest_dir.into(),
            fallback_threshold: Duration::from_millis(self.local_fallback_threshold),
        })
    }
}

#[derive(Debug, Parser)]
pub struct PseudoPeerCommand {
    #[command(flatten)]
    pub logs: LogArgs,

    #[command(flatten)]
    pub source: BlockSourceArgs,

    /// Destination peer to connect to.
    /// Example: enode://412...1a@0.0.0.0:30304
    #[arg(long)]
    pub destination_peer: String,
}
