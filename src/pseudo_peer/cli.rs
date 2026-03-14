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

    /// Interval for polling new blocks in S3 in milliseconds.
    #[arg(id = "s3.polling-interval", long = "s3.polling-interval", default_value = "25")]
    s3_polling_interval: u64,

    /// Interval for polling new blocks from RPC source in milliseconds.
    #[arg(id = "rpc.polling-interval", long = "rpc.polling-interval", default_value = "100")]
    rpc_polling_interval: u64,

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
    pub async fn parse(&self) -> eyre::Result<Option<BlockSourceConfig>> {
        let Some(config) = self.create_base_config().await? else {
            return Ok(None);
        };
        let config = self.apply_node_source_config(config);
        Ok(Some(config))
    }

    async fn create_base_config(&self) -> eyre::Result<Option<BlockSourceConfig>> {
        if self.s3 {
            return Ok(Some(
                BlockSourceConfig::s3_default(Duration::from_millis(self.s3_polling_interval))
                    .await,
            ));
        }

        if self.local {
            return Ok(Some(BlockSourceConfig::local_default()));
        }

        let Some(value) = self.block_source.as_ref() else {
            // No block source specified - node will sync from P2P peers only
            return Ok(None);
        };

        if let Some(bucket) = value.strip_prefix("s3://") {
            Ok(Some(
                BlockSourceConfig::s3(
                    bucket.to_string(),
                    Duration::from_millis(self.s3_polling_interval),
                )
                .await,
            ))
        } else if let Some(url) = value.strip_prefix("rpc://") {
            let url = if url.starts_with("http://") || url.starts_with("https://") {
                url.to_string()
            } else {
                format!("http://{url}")
            };
            Ok(Some(BlockSourceConfig::rpc(url, Duration::from_millis(self.rpc_polling_interval))))
        } else {
            Ok(Some(BlockSourceConfig::local(value.into())))
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
