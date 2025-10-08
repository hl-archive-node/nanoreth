use crate::chainspec::{hl::hl_testnet, HlChainSpec};

use super::hl::hl_mainnet;
use reth_cli::chainspec::ChainSpecParser;
use std::sync::Arc;

/// Chains supported by HyperEVM. First value should be used as the default.
pub const SUPPORTED_CHAINS: &[&str] = &["mainnet", "testnet"];

/// Hyperliquid chain specification parser.
#[derive(Debug, Clone, Default)]
pub struct HlChainSpecParser;

impl ChainSpecParser for HlChainSpecParser {
    type ChainSpec = HlChainSpec;

    const SUPPORTED_CHAINS: &'static [&'static str] = SUPPORTED_CHAINS;

    fn parse(s: &str) -> eyre::Result<Arc<HlChainSpec>> {
        chain_value_parser(s)
    }
}

/// Clap value parser for [`HlChainSpec`]s.
///
/// Currently only mainnet is supported.
pub fn chain_value_parser(s: &str) -> eyre::Result<Arc<HlChainSpec>> {
    match s {
        "mainnet" => Ok(Arc::new(HlChainSpec { inner: hl_mainnet() })),
        "testnet" => Ok(Arc::new(HlChainSpec { inner: hl_testnet() })),
        _ => Err(eyre::eyre!("Unsupported chain: {}", s)),
    }
}
