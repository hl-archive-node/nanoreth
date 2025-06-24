//! EVM config for vanilla ethereum.
//!
//! # Revm features
//!
//! This crate does __not__ enforce specific revm features such as `blst` or `c-kzg`, which are
//! critical for revm's evm internals, it is the responsibility of the implementer to ensure the
//! proper features are selected.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::sync::Arc;
use alloy_consensus::{BlockHeader, Header};
use alloy_evm::eth::EthEvmContext;
pub use alloy_evm::EthEvm;
use alloy_primitives::Address;
use alloy_primitives::U256;
use core::{convert::Infallible, fmt::Debug};
use parking_lot::RwLock;
use reth_chainspec::{ChainSpec, EthChainSpec, MAINNET};
use reth_evm::Database;
use reth_evm::{ConfigureEvm, ConfigureEvmEnv, EvmEnv, EvmFactory, NextBlockEnvAttributes};
use reth_hyperliquid_types::{PrecompilesCache, ReadPrecompileInput, ReadPrecompileResult};
use reth_node_builder::HyperliquidSharedState;
use reth_primitives::SealedBlock;
use reth_primitives::TransactionSigned;
use reth_revm::context::result::{EVMError, HaltReason};
use reth_revm::handler::EthPrecompiles;
use reth_revm::inspector::NoOpInspector;
use reth_revm::interpreter::interpreter::EthInterpreter;
use reth_revm::MainBuilder;
use reth_revm::{
    context::{BlockEnv, CfgEnv, TxEnv},
    context_interface::block::BlobExcessGasAndPrice,
    specification::hardfork::SpecId,
};
use reth_revm::{Context, Inspector, MainContext};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

mod config;
mod fix;
use alloy_eips::eip1559::INITIAL_BASE_FEE;
pub use config::{revm_spec, revm_spec_by_timestamp_and_block_number};
use reth_ethereum_forks::EthereumHardfork;

pub mod execute;

/// Ethereum DAO hardfork state change data.
pub mod dao_fork;

/// [EIP-6110](https://eips.ethereum.org/EIPS/eip-6110) handling.
pub mod eip6110;

/// Ethereum-related EVM configuration.
#[derive(Debug, Clone)]

pub struct EthEvmConfig {
    chain_spec: Arc<ChainSpec>,
    evm_factory: HyperliquidEvmFactory,
    ingest_dir: Option<PathBuf>,
    shared_state: Option<HyperliquidSharedState>,
}

impl EthEvmConfig {
    /// Creates a new Ethereum EVM configuration with the given chain spec.
    pub fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self { chain_spec, ingest_dir: None, evm_factory: Default::default(), shared_state: None }
    }

    pub fn with_ingest_dir(mut self, ingest_dir: PathBuf) -> Self {
        self.ingest_dir = Some(ingest_dir.clone());
        self.evm_factory.ingest_dir = Some(ingest_dir);
        self
    }

    pub fn with_shared_state(mut self, shared_state: Option<HyperliquidSharedState>) -> Self {
        self.shared_state = shared_state.clone();
        self.evm_factory.shared_state = shared_state;
        self
    }

    /// Creates a new Ethereum EVM configuration for the ethereum mainnet.
    pub fn mainnet() -> Self {
        Self::new(MAINNET.clone())
    }

    /// Returns the chain spec associated with this configuration.
    pub const fn chain_spec(&self) -> &Arc<ChainSpec> {
        &self.chain_spec
    }
}

impl ConfigureEvmEnv for EthEvmConfig {
    type Header = Header;
    type Transaction = TransactionSigned;
    type Error = Infallible;
    type TxEnv = TxEnv;
    type Spec = SpecId;

    fn evm_env(&self, header: &Self::Header) -> EvmEnv {
        let spec = config::revm_spec(self.chain_spec(), header);

        // configure evm env based on parent block
        let mut cfg_env = CfgEnv::new().with_chain_id(self.chain_spec.chain().id()).with_spec(spec);
        // this one is effective; todo: disable after system transaction
        cfg_env.disable_base_fee = true;
        cfg_env.disable_eip3607 = true;

        let block_env = BlockEnv {
            number: header.number(),
            beneficiary: header.beneficiary(),
            timestamp: header.timestamp(),
            difficulty: if spec >= SpecId::MERGE { U256::ZERO } else { header.difficulty() },
            prevrandao: if spec >= SpecId::MERGE { header.mix_hash() } else { None },
            gas_limit: header.gas_limit(),
            basefee: header.base_fee_per_gas().unwrap_or_default(),
            // EIP-4844 excess blob gas of this block, introduced in Cancun
            blob_excess_gas_and_price: header.excess_blob_gas.map(|excess_blob_gas| {
                BlobExcessGasAndPrice::new(excess_blob_gas, spec >= SpecId::PRAGUE)
            }),
        };

        EvmEnv { cfg_env, block_env }
    }

    fn next_evm_env(
        &self,
        parent: &Self::Header,
        attributes: NextBlockEnvAttributes,
    ) -> Result<EvmEnv, Self::Error> {
        // ensure we're not missing any timestamp based hardforks
        let spec_id = revm_spec_by_timestamp_and_block_number(
            &self.chain_spec,
            attributes.timestamp,
            parent.number() + 1,
        );

        // configure evm env based on parent block
        let cfg = CfgEnv::new().with_chain_id(self.chain_spec.chain().id()).with_spec(spec_id);

        // if the parent block did not have excess blob gas (i.e. it was pre-cancun), but it is
        // cancun now, we need to set the excess blob gas to the default value(0)
        let blob_excess_gas_and_price = parent
            .maybe_next_block_excess_blob_gas(
                self.chain_spec.blob_params_at_timestamp(attributes.timestamp),
            )
            .or_else(|| (spec_id == SpecId::CANCUN).then_some(0))
            .map(|gas| BlobExcessGasAndPrice::new(gas, spec_id >= SpecId::PRAGUE));

        let mut basefee = parent.next_block_base_fee(
            self.chain_spec.base_fee_params_at_timestamp(attributes.timestamp),
        );

        let mut gas_limit = attributes.gas_limit;

        // If we are on the London fork boundary, we need to multiply the parent's gas limit by the
        // elasticity multiplier to get the new gas limit.
        if self.chain_spec.fork(EthereumHardfork::London).transitions_at_block(parent.number + 1) {
            let elasticity_multiplier = self
                .chain_spec
                .base_fee_params_at_timestamp(attributes.timestamp)
                .elasticity_multiplier;

            // multiply the gas limit by the elasticity multiplier
            gas_limit *= elasticity_multiplier as u64;

            // set the base fee to the initial base fee from the EIP-1559 spec
            basefee = Some(INITIAL_BASE_FEE)
        }

        let block_env = BlockEnv {
            number: parent.number + 1,
            beneficiary: attributes.suggested_fee_recipient,
            timestamp: attributes.timestamp,
            difficulty: U256::ZERO,
            prevrandao: Some(attributes.prev_randao),
            gas_limit,
            // calculate basefee based on parent block's gas usage
            basefee: basefee.unwrap_or_default(),
            // calculate excess gas based on parent block's blob gas usage
            blob_excess_gas_and_price,
        };

        Ok((cfg, block_env).into())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BlockAndReceipts {
    #[serde(default)]
    pub read_precompile_calls: Vec<(Address, Vec<(ReadPrecompileInput, ReadPrecompileResult)>)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum EvmBlock {
    Reth115(SealedBlock),
}

/// Custom EVM configuration.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct HyperliquidEvmFactory {
    ingest_dir: Option<PathBuf>,
    shared_state: Option<HyperliquidSharedState>,
}

pub(crate) fn collect_s3_block(ingest_path: PathBuf, height: u64) -> Option<BlockAndReceipts> {
    let f = ((height - 1) / 1_000_000) * 1_000_000;
    let s = ((height - 1) / 1_000) * 1_000;
    let path = format!("{}/{f}/{s}/{height}.rmp.lz4", ingest_path.to_string_lossy());
    if std::path::Path::new(&path).exists() {
        let file = std::fs::File::open(path).unwrap();
        let file = std::io::BufReader::new(file);
        let mut decoder = lz4_flex::frame::FrameDecoder::new(file);
        let blocks: Vec<BlockAndReceipts> = rmp_serde::from_read(&mut decoder).unwrap();
        Some(blocks[0].clone())
    } else {
        None
    }
}

pub(crate) fn get_locally_sourced_precompiles_for_height(
    precompiles_cache: PrecompilesCache,
    height: u64,
) -> Option<Vec<(Address, Vec<(ReadPrecompileInput, ReadPrecompileResult)>)>> {
    let mut u_cache = precompiles_cache.lock();
    u_cache.remove(&height)
}

pub(crate) fn collect_block(
    ingest_path: PathBuf,
    shared_state: Option<HyperliquidSharedState>,
    height: u64,
) -> Option<BlockAndReceipts> {
    // Attempt to source precompile from the cache that is shared the binary level with the block
    // ingestor.
    if let Some(shared_state) = shared_state {
        if let Some(calls) =
            get_locally_sourced_precompiles_for_height(shared_state.precompiles_cache, height)
        {
            return Some(BlockAndReceipts { read_precompile_calls: calls });
        }
    }
    // Fallback to s3 always
    collect_s3_block(ingest_path, height)
}

impl EvmFactory<EvmEnv> for HyperliquidEvmFactory {
    type Evm<DB: Database, I: Inspector<EthEvmContext<DB>, EthInterpreter>> =
        EthEvm<DB, I, ReplayPrecompile<EthEvmContext<DB>>>;
    type Tx = TxEnv;
    type Error<DBError: core::error::Error + Send + Sync + 'static> = EVMError<DBError>;
    type HaltReason = HaltReason;
    type Context<DB: Database> = EthEvmContext<DB>;

    fn create_evm<DB: Database>(&self, db: DB, input: EvmEnv) -> Self::Evm<DB, NoOpInspector> {
        let block = collect_block(
            self.ingest_dir.clone().unwrap(),
            self.shared_state.clone(),
            input.block_env.number,
        )
        .expect("Failed to collect a submitted block. If sourcing locally, make sure your local hl-node is producing blocks.");
        let cache = block.read_precompile_calls;

        let evm = Context::mainnet()
            .with_db(db)
            .with_cfg(input.cfg_env)
            .with_block(input.block_env)
            .build_mainnet_with_inspector(NoOpInspector {})
            .with_precompiles(ReplayPrecompile::new(
                EthPrecompiles::default(),
                Arc::new(RwLock::new(
                    cache
                        .into_iter()
                        .map(|(address, calls)| (address, HashMap::from_iter(calls.into_iter())))
                        .collect(),
                )),
            ));

        EthEvm::new(evm, false)
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>, EthInterpreter>>(
        &self,
        db: DB,
        input: EvmEnv,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        EthEvm::new(self.create_evm(db, input).into_inner().with_inspector(inspector), true)
    }
}

impl ConfigureEvm for EthEvmConfig {
    type EvmFactory = HyperliquidEvmFactory;

    fn evm_factory(&self) -> &Self::EvmFactory {
        &self.evm_factory
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use alloy_genesis::Genesis;
    use reth_chainspec::{Chain, ChainSpec, MAINNET};
    use reth_evm::{execute::ProviderError, EvmEnv};
    use reth_revm::{
        context::{BlockEnv, CfgEnv},
        database_interface::EmptyDBTyped,
        db::CacheDB,
        inspector::NoOpInspector,
    };

    #[test]
    fn test_fill_cfg_and_block_env() {
        // Create a default header
        let header = Header::default();

        // Build the ChainSpec for Ethereum mainnet, activating London, Paris, and Shanghai
        // hardforks
        let chain_spec = ChainSpec::builder()
            .chain(Chain::mainnet())
            .genesis(Genesis::default())
            .london_activated()
            .paris_activated()
            .shanghai_activated()
            .build();

        // Use the `EthEvmConfig` to fill the `cfg_env` and `block_env` based on the ChainSpec,
        // Header, and total difficulty
        let EvmEnv { cfg_env, .. } =
            EthEvmConfig::new(Arc::new(chain_spec.clone())).evm_env(&header);

        // Assert that the chain ID in the `cfg_env` is correctly set to the chain ID of the
        // ChainSpec
        assert_eq!(cfg_env.chain_id, chain_spec.chain().id());
    }

    #[test]
    fn test_evm_with_env_default_spec() {
        let evm_config = EthEvmConfig::new(MAINNET.clone());

        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        let evm_env = EvmEnv::default();

        let evm = evm_config.evm_with_env(db, evm_env.clone());

        // Check that the EVM environment
        assert_eq!(evm.block, evm_env.block_env);
        assert_eq!(evm.cfg, evm_env.cfg_env);
    }

    #[test]
    fn test_evm_with_env_custom_cfg() {
        let evm_config = EthEvmConfig::new(MAINNET.clone());

        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        // Create a custom configuration environment with a chain ID of 111
        let cfg = CfgEnv::default().with_chain_id(111);

        let evm_env = EvmEnv { cfg_env: cfg.clone(), ..Default::default() };

        let evm = evm_config.evm_with_env(db, evm_env);

        // Check that the EVM environment is initialized with the custom environment
        assert_eq!(evm.cfg, cfg);
    }

    #[test]
    fn test_evm_with_env_custom_block_and_tx() {
        let evm_config = EthEvmConfig::new(MAINNET.clone());

        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        // Create customs block and tx env
        let block =
            BlockEnv { basefee: 1000, gas_limit: 10_000_000, number: 42, ..Default::default() };

        let evm_env = EvmEnv { block_env: block, ..Default::default() };

        let evm = evm_config.evm_with_env(db, evm_env.clone());

        // Verify that the block and transaction environments are set correctly
        assert_eq!(evm.block, evm_env.block_env);

        // Default spec ID
        assert_eq!(evm.cfg.spec, SpecId::LATEST);
    }

    #[test]
    fn test_evm_with_spec_id() {
        let evm_config = EthEvmConfig::new(MAINNET.clone());

        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        let evm_env = EvmEnv {
            cfg_env: CfgEnv::new().with_spec(SpecId::CONSTANTINOPLE),
            ..Default::default()
        };

        let evm = evm_config.evm_with_env(db, evm_env);

        // Check that the spec ID is setup properly
        assert_eq!(evm.cfg.spec, SpecId::CONSTANTINOPLE);
    }

    #[test]
    fn test_evm_with_env_and_default_inspector() {
        let evm_config = EthEvmConfig::new(MAINNET.clone());
        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        let evm_env = EvmEnv::default();

        let evm = evm_config.evm_with_env_and_inspector(db, evm_env.clone(), NoOpInspector {});

        // Check that the EVM environment is set to default values
        assert_eq!(evm.block, evm_env.block_env);
        assert_eq!(evm.cfg, evm_env.cfg_env);
    }

    #[test]
    fn test_evm_with_env_inspector_and_custom_cfg() {
        let evm_config = EthEvmConfig::new(MAINNET.clone());
        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        let cfg_env = CfgEnv::default().with_chain_id(111);
        let block = BlockEnv::default();
        let evm_env = EvmEnv { cfg_env: cfg_env.clone(), block_env: block };

        let evm = evm_config.evm_with_env_and_inspector(db, evm_env, NoOpInspector {});

        // Check that the EVM environment is set with custom configuration
        assert_eq!(evm.cfg, cfg_env);
        assert_eq!(evm.cfg.spec, SpecId::LATEST);
    }

    #[test]
    fn test_evm_with_env_inspector_and_custom_block_tx() {
        let evm_config = EthEvmConfig::new(MAINNET.clone());
        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        // Create custom block and tx environment
        let block =
            BlockEnv { basefee: 1000, gas_limit: 10_000_000, number: 42, ..Default::default() };
        let evm_env = EvmEnv { block_env: block, ..Default::default() };

        let evm = evm_config.evm_with_env_and_inspector(db, evm_env.clone(), NoOpInspector {});

        // Verify that the block and transaction environments are set correctly
        assert_eq!(evm.block, evm_env.block_env);
        assert_eq!(evm.cfg.spec, SpecId::LATEST);
    }

    #[test]
    fn test_evm_with_env_inspector_and_spec_id() {
        let evm_config = EthEvmConfig::new(MAINNET.clone());
        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        let evm_env = EvmEnv {
            cfg_env: CfgEnv::new().with_spec(SpecId::CONSTANTINOPLE),
            ..Default::default()
        };

        let evm = evm_config.evm_with_env_and_inspector(db, evm_env.clone(), NoOpInspector {});

        // Check that the spec ID is set properly
        assert_eq!(evm.block, evm_env.block_env);
        assert_eq!(evm.cfg, evm_env.cfg_env);
        assert_eq!(evm.tx, Default::default());
    }
}

mod precompile_replay;

pub use precompile_replay::ReplayPrecompile;
