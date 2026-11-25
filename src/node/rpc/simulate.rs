use super::{HlEthApi, HlRpcNodeCore};
use crate::{
    HlPrimitives,
    node::evm::{config::HlEvmConfig, mrb, read_precompile_forwarder::read_precompile_forwarder},
};
use alloy_consensus::BlockHeader;
use alloy_eips::BlockId;
use alloy_evm::overrides::{OverrideBlockHashes, apply_block_overrides, apply_state_overrides};
use alloy_primitives::B256;
use alloy_rpc_types_eth::simulate::{SimBlock, SimulatePayload, SimulatedBlock};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_errors::RethError;
use reth_evm::{ConfigureEvm, Evm, env::BlockEnvironment, execute::BlockBuilder};
use reth_revm::{
    database::StateProviderDatabase,
    db::{State, bal::BalState},
};
use reth_rpc_convert::{RpcConvert, RpcTxReq};
use reth_rpc_eth_api::{
    EthApiTypes, FromEthApiError, RpcBlock, RpcNodeCore,
    helpers::{
        Call, EthCall, LoadBlock, LoadPendingBlock, SpawnBlocking, call::SimulatedBlocksResult,
    },
};
use reth_rpc_eth_types::{
    EthApiError,
    error::AsEthApiError,
    simulate::{self, EthSimulateError},
};
use revm::{context::Block as _, context_interface::Cfg};
use revm_inspectors::transfer::TransferInspector;
use std::{collections::BTreeMap, future::Future};

// Concrete over `HlEvmConfig`: with a generic EVM, the `EthApiError: FromEvmError<N::Evm>` bound
// puts `EthApiError: From<EvmError>` in scope as a where-clause, which then wins inference for
// every `EthApiError` conversion in the upstream body below.
impl<N, Rpc> EthCall for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore<Primitives = HlPrimitives, Evm = HlEvmConfig>,
    Rpc: RpcConvert<Primitives = HlPrimitives, Error = EthApiError, Evm = HlEvmConfig>,
{
    /// Copy of reth's default `simulate_v1` (v2.8.1), with the read precompile data attached to
    /// the execution context and the MRB spec selected for overridden block numbers. The HL
    /// additions are marked `HL:`; everything else should track upstream.
    ///
    /// Simulation runs through the block builder rather than through [`Call::transact`], so it
    /// picks up its precompiles from [`HlBlockExecutionCtx`] instead. That context is built by
    /// `context_for_next_block`, which has no provider to look the head up with and so leaves the
    /// precompile address range empty — without this override, the precompiles above the default
    /// `0x80d` (`bbo` and up) are not installed at all and simulating a call that reads them
    /// fails as though they were plain accounts.
    ///
    /// [`HlBlockExecutionCtx`]: crate::node::evm::config::HlBlockExecutionCtx
    // The trait declares this as `-> impl Future + Send`, which `async fn` would not reproduce.
    #[allow(clippy::manual_async_fn)]
    fn simulate_v1(
        &self,
        payload: SimulatePayload<RpcTxReq<<Self::RpcConvert as RpcConvert>::Network>>,
        block: Option<BlockId>,
    ) -> impl Future<Output = SimulatedBlocksResult<Self::NetworkTypes, Self::Error>> + Send {
        async move {
            if payload.block_state_calls.len() > self.max_simulate_blocks() as usize {
                return Err(EthApiError::other(EthSimulateError::TooManyBlocks));
            }

            let block = block.unwrap_or_default();

            let SimulatePayload {
                block_state_calls,
                trace_transfers,
                validation,
                return_full_transactions,
            } = payload;

            if block_state_calls.is_empty() {
                return Err(EthApiError::InvalidParams(String::from("calls are empty.")));
            }

            let permit = self
                .acquire_owned_blocking_io()
                .await
                .map_err(|_| EthApiError::InternalEthError)?;

            let base_block = self
                .recovered_block(block)
                .await?
                .ok_or_else(|| EthApiError::other(EthSimulateError::BlockNotFound { block }))?;
            let parent = base_block.sealed_header().clone();
            let max_simulate_blocks = self.max_simulate_blocks();
            // Load the state of the block resolved above: resolving a tag such as `latest` again
            // could return a newer block than `parent`.
            let state_at = if block.is_pending() { block } else { parent.hash().into() };

            // HL: simulated blocks inherit the base block's read precompile address range and,
            // when simulating from the head, a forwarder. See `hl_simulation_precompiles`.
            let simulation_precompiles = if read_precompile_forwarder().is_some() {
                Some(self.hl_simulation_precompiles(parent.number())?)
            } else {
                None
            };

            self.spawn_with_state_at_block(state_at, move |this, db| {
                let _permit = permit;
                let state_provider = db.database.into_inner();
                let mut db = State::builder()
                    .with_database(StateProviderDatabase::new(&state_provider))
                    .with_bundle_update()
                    .build();
                let mut parent = parent;

                let chain_id = this.provider().chain_spec().chain_id();

                // Validate block ordering and fill gaps with empty blocks so every entry has an
                // explicit `number` and `time` override and the chain is contiguous (see the
                // execution-apis spec note: "If the block number is increased more than 1 compared
                // to the previous block, new empty blocks are generated in between.").
                let block_state_calls = simulate::sanitize_chain(
                    block_state_calls,
                    &parent,
                    chain_id,
                    max_simulate_blocks,
                )?;

                let mut blocks: Vec<SimulatedBlock<RpcBlock<Self::NetworkTypes>>> =
                    Vec::with_capacity(block_state_calls.len());

                let call_gas_limit = this.call_gas_limit();
                let mut remaining_call_gas_limit = (call_gas_limit > 0).then_some(call_gas_limit);

                for block in block_state_calls {
                    let SimBlock { block_overrides, state_overrides, calls } = block;

                    let attributes = this
                        .pending_env_builder()
                        .pending_env_attributes(&parent, block_overrides.as_ref())
                        .map_err(Self::Error::from_eth_err)?;

                    let mut evm_env = this
                        .evm_config()
                        .next_evm_env(&parent, &attributes)
                        .map_err(RethError::other)
                        .map_err(Self::Error::from_eth_err)?;

                    // Always disable EIP-3607
                    evm_env.cfg_env.disable_eip3607 = true;

                    // EIP-7825's transaction gas cap is only active with Amsterdam's
                    // regular/state-gas accounting, and only in validation mode. Without
                    // validation the whole gas limit is execution gas.
                    if !validation || !evm_env.cfg_env.is_amsterdam_eip8037_enabled() {
                        evm_env.cfg_env.tx_gas_limit_cap = Some(u64::MAX);
                    }

                    if !validation {
                        // If not explicitly required, we disable nonce check <https://github.com/paradigmxyz/reth/issues/16108>
                        evm_env.cfg_env.disable_nonce_check = true;
                        evm_env.cfg_env.disable_base_fee = true;
                        evm_env.block_env.inner_mut().basefee = 0;
                    }

                    // Set prevrandao to zero for simulated blocks by default,
                    // matching spec behavior where MixDigest is zero-initialized.
                    // If user provides an override, it will be applied by apply_block_overrides.
                    evm_env.block_env.inner_mut().prevrandao = Some(B256::ZERO);
                    if !this
                        .provider()
                        .chain_spec()
                        .is_paris_active_at_block(evm_env.block_env.number().saturating_to())
                    {
                        evm_env.block_env.inner_mut().difficulty = parent.difficulty();
                    }

                    if let Some(block_overrides) = block_overrides {
                        // ensure we don't allow uncapped gas limit per block
                        if let Some(gas_limit_override) = block_overrides.gas_limit
                            && gas_limit_override > evm_env.block_env.gas_limit()
                            && gas_limit_override > this.call_gas_limit()
                        {
                            return Err(EthApiError::other(EthSimulateError::GasLimitReached));
                        }
                        apply_block_overrides(
                            block_overrides,
                            &mut db,
                            evm_env.block_env.inner_mut(),
                        );
                        // HL: precompile prices follow the block number, so an overridden number picks
                        // its spec.
                        evm_env.cfg_env.spec = mrb::spec_at(
                            evm_env.cfg_env.chain_id,
                            evm_env.block_env.number().saturating_to(),
                        );
                    }
                    if let Some(ref state_overrides) = state_overrides {
                        apply_state_overrides(state_overrides.clone(), &mut db)
                            .map_err(Self::Error::from_eth_err)?;
                    }

                    let chain_id = evm_env.cfg_env.chain_id;

                    // Each simulated block needs its own BAL, including when crossing Amsterdam.
                    if this.provider().chain_spec().is_amsterdam_active_at_timestamp(
                        evm_env.block_env.timestamp().saturating_to(),
                    ) {
                        db.bal_state = BalState::new().with_bal_builder();
                    }

                    let mut ctx = this
                        .evm_config()
                        .context_for_next_block(&parent, attributes)
                        .map_err(RethError::other)
                        .map_err(Self::Error::from_eth_err)?;
                    // HL: attach the inherited read precompile data. Simulated blocks must not borrow
                    // recorded calls from canonical blocks that happen to occupy the same heights.
                    if let Some((simulation_extras, simulation_forwarder)) = &simulation_precompiles
                    {
                        ctx.extras = simulation_extras.clone();
                        ctx.enable_rpc_read_precompile_forwarding(simulation_forwarder.clone());
                    }
                    let map_err = |e: EthApiError| -> Self::Error {
                        match e.as_simulate_error() {
                            Some(sim_err) => Self::Error::from_eth_err(EthApiError::other(sim_err)),
                            None => Self::Error::from_eth_err(e),
                        }
                    };

                    // EIP-7708 already emits transfer logs: https://eips.ethereum.org/EIPS/eip-7708
                    let trace_transfers = trace_transfers
                        && (!revm::primitives::hardfork::SpecId::from(Cfg::spec(&evm_env.cfg_env))
                            .is_enabled_in(revm::primitives::hardfork::SpecId::AMSTERDAM)
                            || evm_env.cfg_env.is_eip7708_disabled());
                    let (result, results) = if trace_transfers {
                        // prepare inspector to capture transfer inside the evm so they are recorded
                        // and included in logs
                        let inspector = TransferInspector::new(false).with_logs(true);
                        let evm = this
                            .evm_config()
                            .evm_with_env_and_inspector(&mut db, evm_env, inspector);
                        let mut builder = this.evm_config().create_block_builder(evm, &parent, ctx);

                        if let Some(ref state_overrides) = state_overrides {
                            simulate::apply_precompile_overrides(
                                state_overrides,
                                builder.evm_mut().precompiles_mut(),
                            )
                            .map_err(|e| Self::Error::from_eth_err(EthApiError::other(e)))?;
                        }

                        simulate::execute_transactions(
                            builder,
                            &*state_provider,
                            calls,
                            &mut remaining_call_gas_limit,
                            chain_id,
                            this.compute_state_root_for_eth_simulate(),
                            this.converter(),
                        )
                        .map_err(map_err)?
                    } else {
                        let evm = this.evm_config().evm_with_env(&mut db, evm_env);
                        let mut builder = this.evm_config().create_block_builder(evm, &parent, ctx);

                        if let Some(ref state_overrides) = state_overrides {
                            simulate::apply_precompile_overrides(
                                state_overrides,
                                builder.evm_mut().precompiles_mut(),
                            )
                            .map_err(|e| Self::Error::from_eth_err(EthApiError::other(e)))?;
                        }

                        simulate::execute_transactions(
                            builder,
                            &*state_provider,
                            calls,
                            &mut remaining_call_gas_limit,
                            chain_id,
                            this.compute_state_root_for_eth_simulate(),
                            this.converter(),
                        )
                        .map_err(map_err)?
                    };

                    let simulated_header = result.block.clone_sealed_header();
                    db.override_block_hashes(BTreeMap::from([(
                        simulated_header.number(),
                        simulated_header.hash(),
                    )]));
                    parent = simulated_header;

                    let block = simulate::build_simulated_block::<Self::Error, _>(
                        result.block,
                        results,
                        return_full_transactions.into(),
                        this.converter(),
                    )?;

                    blocks.push(block);
                }

                Ok(blocks)
            })
            .await
        }
    }
}
