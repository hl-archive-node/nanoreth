use super::{HlEthApi, HlRpcNodeCore, apply_precompiles};
use alloy_evm::overrides::{StateOverrideError, apply_block_overrides, apply_state_overrides};
use alloy_network::TransactionBuilder;
use alloy_primitives::{TxKind, U256};
use alloy_rpc_types_eth::state::EvmOverrides;
use reth_chainspec::MIN_TRANSACTION_GAS;
use reth_evm::{ConfigureEvm, Evm, EvmEnvFor, TransactionEnvMut, env::BlockEnvironment};
use reth_errors::ProviderError;
use reth_revm::{
    database::{EvmStateProvider, StateProviderDatabase},
    db::{State, bal::EvmDatabaseError},
};
use reth_rpc_convert::{RpcConvert, RpcTxReq};
use reth_rpc_eth_api::{
    AsEthApiError, FromEthApiError, IntoEthApiError, RpcNodeCore,
    helpers::{
        Call,
        estimate::{EstimateCall, update_estimated_gas_range},
    },
};
use reth_rpc_eth_types::{
    EthApiError, RpcInvalidTransactionError,
    error::{
        FromEvmError,
        api::{FromEvmHalt, FromRevert},
    },
};
use reth_rpc_server_types::constants::gas_oracle::{CALL_STIPEND_GAS, ESTIMATE_GAS_ERROR_RATIO};
use revm::{
    context::Block,
    context_interface::{Cfg, Transaction, result::ExecutionResult},
    primitives::KECCAK_EMPTY,
};
use tracing::trace;

impl<N, Rpc> EstimateCall for HlEthApi<N, Rpc>
where
    Self: Call,
    N: HlRpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
{
    // Modified version of the upstream implementation that adds `apply_precompiles`; upstream
    // comments are stripped out.
    fn estimate_gas_with<S>(
        &self,
        mut evm_env: EvmEnvFor<Self::Evm>,
        mut request: RpcTxReq<<Self::RpcConvert as RpcConvert>::Network>,
        state: S,
        overrides: EvmOverrides,
    ) -> Result<U256, Self::Error>
    where
        S: EvmStateProvider,
    {
        evm_env.cfg_env.disable_eip3607 = true;
        evm_env.cfg_env.disable_base_fee = true;
        evm_env.cfg_env.disable_fee_charge = true;

        request.as_mut().take_nonce();

        let tx_request_gas_limit = request.as_ref().gas_limit();
        let tx_request_gas_price = request.as_ref().gas_price();

        let mut db = State::builder().with_database(StateProviderDatabase::new(state)).build();

        if let Some(block_overrides) = overrides.block {
            apply_block_overrides(*block_overrides, &mut db, evm_env.block_env.inner_mut());
        }

        if let Some(state_override) = overrides.state {
            apply_state_overrides(state_override, &mut db).map_err(
                |err: StateOverrideError<EvmDatabaseError<ProviderError>>| {
                    EthApiError::from_eth_err::<
                        StateOverrideError<EvmDatabaseError<ProviderError>>,
                    >(err)
                },
            )?;
        }

        let block_gas_limit = evm_env.block_env.gas_limit();
        let max_gas_limit = if evm_env.cfg_env.is_amsterdam_eip8037_enabled() {
            block_gas_limit
        } else {
            evm_env.cfg_env.tx_gas_limit_cap().min(block_gas_limit)
        };

        let mut highest_gas_limit = tx_request_gas_limit
            .map(|mut tx_gas_limit| {
                if max_gas_limit < tx_gas_limit {
                    tx_gas_limit = max_gas_limit;
                }
                tx_gas_limit
            })
            .unwrap_or(max_gas_limit);

        let mut tx_env = self.create_txn_env(&evm_env, request, &mut db)?;

        let is_basic_transfer = if tx_env.input().is_empty() &&
            let TxKind::Call(to) = tx_env.kind()
        {
            match db.database.basic_account(&to) {
                Ok(Some(account)) => {
                    account.bytecode_hash.is_none() || account.bytecode_hash == Some(KECCAK_EMPTY)
                }
                _ => true,
            }
        } else {
            false
        };

        if tx_env.gas_price() > 0 {
            highest_gas_limit =
                highest_gas_limit.min(self.caller_gas_allowance(&mut db, &evm_env, &tx_env)?);
        }

        tx_env.set_gas_limit(tx_env.gas_limit().min(highest_gas_limit));

        let block_number: u64 = evm_env.block_env.number().saturating_to();
        let hl_extras = self.get_hl_extras(block_number.into())?;

        let mut evm = self.evm_config().evm_with_env(&mut db, evm_env);
        apply_precompiles(&mut evm, &hl_extras);

        if is_basic_transfer {
            let mut min_tx_env = tx_env.clone();
            min_tx_env.set_gas_limit(MIN_TRANSACTION_GAS);

            if let Ok(res) = evm.transact(min_tx_env).map_err(Self::Error::from_evm_err) &&
                res.result.is_success()
            {
                return Ok(U256::from(MIN_TRANSACTION_GAS));
            }
        }

        trace!(target: "rpc::eth::estimate", ?tx_env, gas_limit = tx_env.gas_limit(), is_basic_transfer, "Starting gas estimation");

        let mut res = match evm.transact(tx_env.clone()).map_err(Self::Error::from_evm_err) {
            Err(err)
                if err.is_gas_too_high() &&
                    (tx_request_gas_limit.is_some() || tx_request_gas_price.is_some()) =>
            {
                    // Inlined `map_out_of_gas_err`: its `DB` bound does not unify against the
                    // concrete `State<StateProviderDatabase<S>>` database used here.
                    let req_gas_limit = tx_env.gas_limit();
                    tx_env.set_gas_limit(max_gas_limit);
                    let retry_res =
                        evm.transact(tx_env).map_err(Self::Error::from_evm_err)?;
                    return match retry_res.result {
                        ExecutionResult::Success { .. } => Err(
                            RpcInvalidTransactionError::BasicOutOfGas(req_gas_limit).into_eth_err(),
                        ),
                        ExecutionResult::Revert { output, .. } => {
                            Err(Self::Error::from_revert(output))
                        }
                        ExecutionResult::Halt { reason, .. } => {
                            Err(Self::Error::from_evm_halt(reason, req_gas_limit))
                        }
                    };
                }
            Err(err) if err.is_gas_too_low() => {
                return Err(RpcInvalidTransactionError::GasRequiredExceedsAllowance {
                    gas_limit: tx_env.gas_limit(),
                }
                .into_eth_err());
            }

            ethres => ethres?,
        };

        let gas_refund = match res.result {
            ExecutionResult::Success { gas, .. } => gas.final_refunded(),
            ExecutionResult::Halt { reason, .. } => {
                return Err(Self::Error::from_evm_halt(reason, tx_env.gas_limit()));
            }
            ExecutionResult::Revert { output, .. } => {
                if tx_request_gas_limit.is_some() || tx_request_gas_price.is_some() {
                    let req_gas_limit = tx_env.gas_limit();
                    tx_env.set_gas_limit(max_gas_limit);
                    let retry_res = evm.transact(tx_env).map_err(Self::Error::from_evm_err)?;
                    return match retry_res.result {
                        ExecutionResult::Success { .. } => Err(
                            RpcInvalidTransactionError::BasicOutOfGas(req_gas_limit).into_eth_err(),
                        ),
                        ExecutionResult::Revert { output, .. } => {
                            Err(Self::Error::from_revert(output))
                        }
                        ExecutionResult::Halt { reason, .. } => {
                            Err(Self::Error::from_evm_halt(reason, req_gas_limit))
                        }
                    };
                }
                return Err(Self::Error::from_revert(output));
            }
        };

        highest_gas_limit = tx_env.gas_limit();

        let mut gas_used = res.result.tx_gas_used();

        let mut lowest_gas_limit = gas_used.saturating_sub(1);

        let optimistic_gas_limit = (gas_used + gas_refund + CALL_STIPEND_GAS) * 64 / 63;
        if optimistic_gas_limit < highest_gas_limit {
            let mut optimistic_tx_env = tx_env.clone();
            optimistic_tx_env.set_gas_limit(optimistic_gas_limit);

            res = evm.transact(optimistic_tx_env).map_err(Self::Error::from_evm_err)?;

            gas_used = res.result.tx_gas_used();

            update_estimated_gas_range(
                res.result,
                optimistic_gas_limit,
                &mut highest_gas_limit,
                &mut lowest_gas_limit,
            )?;
        };

        let mut mid_gas_limit = std::cmp::min(
            gas_used * 3,
            ((highest_gas_limit as u128 + lowest_gas_limit as u128) / 2) as u64,
        );

        trace!(target: "rpc::eth::estimate", ?highest_gas_limit, ?lowest_gas_limit, ?mid_gas_limit, "Starting binary search for gas");

        while lowest_gas_limit + 1 < highest_gas_limit {
            let ratio = (highest_gas_limit - lowest_gas_limit) as f64 / (highest_gas_limit as f64);
            if ratio < ESTIMATE_GAS_ERROR_RATIO {
                break;
            };

            let mut mid_tx_env = tx_env.clone();
            mid_tx_env.set_gas_limit(mid_gas_limit);

            match evm.transact(mid_tx_env).map_err(Self::Error::from_evm_err) {
                Err(err) if err.is_gas_too_high() => {
                    highest_gas_limit = mid_gas_limit;
                }
                Err(err) if err.is_gas_too_low() => {
                    lowest_gas_limit = mid_gas_limit;
                }

                ethres => {
                    res = ethres?;

                    update_estimated_gas_range(
                        res.result,
                        mid_gas_limit,
                        &mut highest_gas_limit,
                        &mut lowest_gas_limit,
                    )?;
                }
            }

            mid_gas_limit = ((highest_gas_limit as u128 + lowest_gas_limit as u128) / 2) as u64;
        }

        Ok(U256::from(highest_gas_limit))
    }
}
