use super::{HlEthApi, HlRpcNodeCore};
use crate::{HlHeader, evm::transaction::HlTxEnvExt, node::evm::config::HlBlockExecutionCtx};
use alloy_eips::BlockId;
use alloy_evm::{
    EvmFactory,
    overrides::{StateOverrideError, apply_block_overrides, apply_state_overrides},
};
use alloy_network::TransactionBuilder;
use alloy_rpc_types_eth::simulate::{SimBlock, SimulatePayload, SimulatedBlock};
use reth_errors::{ProviderError, RethError};
use reth_evm::{ConfigureEvm, SpecFor, TxEnvFor, block::BlockExecutorFactory};
use reth_primitives::NodePrimitives;
use reth_revm::{database::StateProviderDatabase, db::State};
use reth_rpc_convert::{RpcConvert, RpcTxReq};
use reth_rpc_eth_api::{
    EthApiTypes, FromEvmError, RpcBlock, RpcNodeCore,
    helpers::{Call, EthCall, LoadBlock, LoadPendingBlock, call::SimulatedBlocksResult},
};
use reth_rpc_eth_types::{
    EthApiError,
    simulate::{self, EthSimulateError},
};
use revm_inspectors::transfer::TransferInspector;
use std::future::Future;

impl<N, Rpc> EthCall for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore<
            Primitives: NodePrimitives<BlockHeader = HlHeader>,
            Evm: ConfigureEvm<
                BlockExecutorFactory: for<'a> BlockExecutorFactory<
                    ExecutionCtx<'a> = HlBlockExecutionCtx<'a>,
                    EvmFactory: EvmFactory<Tx: HlTxEnvExt>,
                >,
            >,
        >,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<
            Primitives = N::Primitives,
            Error = EthApiError,
            TxEnv = TxEnvFor<N::Evm>,
            Spec = SpecFor<N::Evm>,
        >,
{
    /// Copy of reth's default `simulate_v1`, with the read precompile data attached to the
    /// execution context; comments are stripped out.
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
                return Err(EthApiError::InvalidParams("too many blocks.".to_string()));
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

            let base_block =
                self.recovered_block(block).await?.ok_or(EthApiError::HeaderNotFound(block))?;
            let mut parent = base_block.sealed_header().clone();
            let simulation_precompiles =
                if crate::node::evm::read_precompile_forwarder::read_precompile_forwarder()
                    .is_some()
                {
                    Some(self.hl_simulation_precompiles(parent.number)?)
                } else {
                    None
                };

            let this = self.clone();
            self.spawn_with_state_at_block(block, move |state| {
                let mut db =
                    State::builder().with_database(StateProviderDatabase::new(state)).build();
                let mut blocks: Vec<SimulatedBlock<RpcBlock<Self::NetworkTypes>>> =
                    Vec::with_capacity(block_state_calls.len());
                for block in block_state_calls {
                    let mut evm_env = this
                        .evm_config()
                        .next_evm_env(&parent, &this.next_env_attributes(&parent)?)
                        .map_err(RethError::other)
                        .map_err(<EthApiError as From<RethError>>::from)?;

                    evm_env.cfg_env.disable_eip3607 = true;

                    if !validation {
                        evm_env.cfg_env.disable_nonce_check = true;
                        evm_env.cfg_env.disable_base_fee = true;
                        evm_env.cfg_env.tx_gas_limit_cap = Some(u64::MAX);
                        evm_env.block_env.basefee = 0;
                    }

                    let SimBlock { block_overrides, state_overrides, calls } = block;

                    if let Some(block_overrides) = block_overrides {
                        if let Some(gas_limit_override) = block_overrides.gas_limit
                            && gas_limit_override > evm_env.block_env.gas_limit
                            && gas_limit_override > this.call_gas_limit()
                        {
                            return Err(EthApiError::other(
                                EthSimulateError::GasLimitReached,
                            ));
                        }
                        apply_block_overrides(block_overrides, &mut db, &mut evm_env.block_env);
                    }
                    if let Some(state_overrides) = state_overrides {
                        apply_state_overrides(state_overrides, &mut db).map_err(
                            <EthApiError as From<StateOverrideError<ProviderError>>>::from,
                        )?;
                    }

                    let block_gas_limit = evm_env.block_env.gas_limit;
                    let chain_id = evm_env.cfg_env.chain_id;

                    let default_gas_limit = {
                        let total_specified_gas =
                            calls.iter().filter_map(|tx| tx.as_ref().gas_limit()).sum::<u64>();
                        let txs_without_gas_limit =
                            calls.iter().filter(|tx| tx.as_ref().gas_limit().is_none()).count();

                        if total_specified_gas > block_gas_limit {
                            return Err(EthApiError::Other(Box::new(
                                EthSimulateError::BlockGasLimitExceeded,
                            )));
                        }

                        if txs_without_gas_limit > 0 {
                            (block_gas_limit - total_specified_gas) / txs_without_gas_limit as u64
                        } else {
                            0
                        }
                    };

                    let mut ctx = this
                        .evm_config()
                        .context_for_next_block(&parent, this.next_env_attributes(&parent)?)
                        .map_err(RethError::other)
                        .map_err(<EthApiError as From<RethError>>::from)?;

                    // The only part that differs from reth: attach the read precompile address
                    // range inherited from the base block and, for head simulations, a forwarder.
                    // Simulated blocks must not borrow recorded calls from canonical blocks that
                    // happen to occupy the same heights.
                    if let Some((simulation_extras, simulation_forwarder)) = &simulation_precompiles
                    {
                        ctx.extras = simulation_extras.clone();
                        ctx.enable_rpc_read_precompile_forwarding(simulation_forwarder.clone());
                    }

                    let (result, results) = if trace_transfers {
                        let inspector = TransferInspector::new(false).with_logs(true);
                        let evm = this
                            .evm_config()
                            .evm_with_env_and_inspector(&mut db, evm_env, inspector);
                        let builder = this.evm_config().create_block_builder(evm, &parent, ctx);
                        simulate::execute_transactions(
                            builder,
                            calls,
                            default_gas_limit,
                            chain_id,
                            this.tx_resp_builder(),
                        )?
                    } else {
                        let evm = this.evm_config().evm_with_env(&mut db, evm_env);
                        let builder = this.evm_config().create_block_builder(evm, &parent, ctx);
                        simulate::execute_transactions(
                            builder,
                            calls,
                            default_gas_limit,
                            chain_id,
                            this.tx_resp_builder(),
                        )?
                    };

                    parent = result.block.clone_sealed_header();

                    let block = simulate::build_simulated_block(
                        result.block,
                        results,
                        return_full_transactions.into(),
                        this.tx_resp_builder(),
                    )?;

                    blocks.push(block);
                }

                Ok(blocks)
            })
            .await
        }
    }
}
