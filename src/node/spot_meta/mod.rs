use alloy_primitives::{Address, U256};
use backon::{BlockingRetryable, ExponentialBuilder};
use eyre::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, str::FromStr, sync::RwLock};
use tracing::warn;

use crate::chainspec::{MAINNET_CHAIN_ID, TESTNET_CHAIN_ID};

/// CLI/env override for the spot-meta API URL. When set, it takes precedence over
/// the chain-default URL. Set once at startup; falls back to `SPOT_META_URL` env var.
static URL_OVERRIDE: RwLock<Option<String>> = RwLock::new(None);

/// CLI/env supplied manual `address -> spot index` mappings applied on top of the
/// fetched metadata (in addition to the built-in testnet patch).
static MANUAL_OVERRIDES: RwLock<BTreeMap<Address, u64>> = RwLock::new(BTreeMap::new());

/// Set a custom spot-meta API URL (e.g. from `--spot-meta-url`). `None` clears it.
pub fn set_spot_meta_url(url: Option<String>) {
    *URL_OVERRIDE.write().unwrap() = url.filter(|u| !u.is_empty());
}

/// Add manual `address -> spot index` overrides (e.g. from `--spot-meta-override`).
pub fn add_spot_meta_overrides(overrides: BTreeMap<Address, u64>) {
    MANUAL_OVERRIDES.write().unwrap().extend(overrides);
}

/// Parse a single `address:index` override string into its components.
pub fn parse_spot_meta_override(s: &str) -> Result<(Address, u64)> {
    let (addr, index) = s
        .split_once(':')
        .ok_or_else(|| Error::msg(format!("invalid override '{s}', expected 'address:index'")))?;
    let address = Address::from_str(addr.trim())
        .map_err(|e| Error::msg(format!("invalid address in override '{s}': {e}")))?;
    let index = index
        .trim()
        .parse::<u64>()
        .map_err(|e| Error::msg(format!("invalid index in override '{s}': {e}")))?;
    Ok((address, index))
}

/// Resolve the API URL: CLI/env override first, then the chain default.
fn resolve_url(chain_id: u64) -> Result<String> {
    if let Some(url) = URL_OVERRIDE.read().unwrap().clone() {
        return Ok(url);
    }
    if let Ok(url) = std::env::var("SPOT_META_URL")
        && !url.is_empty()
    {
        return Ok(url);
    }
    match chain_id {
        MAINNET_CHAIN_ID => Ok("https://api.hyperliquid.xyz/info".to_string()),
        TESTNET_CHAIN_ID => Ok("https://api.hyperliquid-testnet.xyz/info".to_string()),
        _ => Err(Error::msg("unknown chain id")),
    }
}

/// Collect manual overrides from the global store and the `SPOT_META_OVERRIDES` env var.
fn collect_manual_overrides() -> BTreeMap<Address, u64> {
    let mut overrides = MANUAL_OVERRIDES.read().unwrap().clone();
    if let Ok(raw) = std::env::var("SPOT_META_OVERRIDES") {
        for entry in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match parse_spot_meta_override(entry) {
                Ok((addr, index)) => {
                    overrides.insert(addr, index);
                }
                Err(e) => warn!("Ignoring invalid SPOT_META_OVERRIDES entry: {e}"),
            }
        }
    }
    overrides
}

pub mod init;
mod patch;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EvmContract {
    address: Address,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SpotToken {
    index: u64,
    #[serde(rename = "evmContract")]
    evm_contract: Option<EvmContract>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpotMeta {
    tokens: Vec<SpotToken>,
}

#[derive(Debug, Clone)]
pub struct SpotId {
    pub index: u64,
}

impl SpotId {
    pub(crate) fn to_s(&self) -> U256 {
        let mut addr = [0u8; 32];
        addr[12] = 0x20;
        addr[24..32].copy_from_slice(self.index.to_be_bytes().as_ref());
        U256::from_be_bytes(addr)
    }
}

fn fetch_spot_meta(chain_id: u64) -> Result<SpotMeta> {
    let url = resolve_url(chain_id)?;

    let fetch = || -> Result<SpotMeta> {
        let response = ureq::post(&url)
            .header("Content-Type", "application/json")
            .send(serde_json::json!({"type": "spotMeta"}).to_string())?
            .into_body()
            .read_to_string()?;
        Ok(serde_json::from_str(&response)?)
    };

    fetch
        .retry(ExponentialBuilder::default().without_max_times().with_jitter())
        .notify(|err, dur| {
            warn!("Failed to fetch spot metadata: {err}. Retrying in {dur:?}");
        })
        .call()
}

pub(crate) fn erc20_contract_to_spot_token(chain_id: u64) -> Result<BTreeMap<Address, SpotId>> {
    let meta = fetch_spot_meta(chain_id)?;
    let mut map = BTreeMap::new();
    for token in &meta.tokens {
        if let Some(evm_contract) = &token.evm_contract {
            map.insert(evm_contract.address, SpotId { index: token.index });
        }
    }

    if chain_id == TESTNET_CHAIN_ID {
        patch::patch_testnet_spot_meta(&mut map);
    }

    // Apply CLI/env manual overrides last so they win over both the fetched
    // metadata and the built-in patches.
    for (address, index) in collect_manual_overrides() {
        map.insert(address, SpotId { index });
    }

    Ok(map)
}
