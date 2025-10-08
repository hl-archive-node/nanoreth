use super::{apply_precompiles, HlEthApi, HlRpcNodeCore};
use alloy_evm::overrides::{apply_state_overrides, StateOverrideError};
use alloy_network::TransactionBuilder;
use alloy_primitives::{TxKind, U256};
use alloy_rpc_types_eth::state::StateOverride;
use reth_chainspec::MIN_TRANSACTION_GAS;
use reth_errors::ProviderError;
use reth_evm::{ConfigureEvm, Evm, EvmEnvFor, SpecFor, TransactionEnv, TxEnvFor};
use reth_revm::{database::StateProviderDatabase, db::CacheDB};
use reth_rpc_convert::{RpcConvert, RpcTxReq};
use reth_rpc_eth_api::{
    helpers::{
        estimate::{update_estimated_gas_range, EstimateCall},
        Call,
    },
    AsEthApiError, IntoEthApiError, RpcNodeCore,
};
use reth_rpc_eth_types::{
    error::{api::FromEvmHalt, FromEvmError},
    EthApiError, RevertError, RpcInvalidTransactionError,
};
use reth_rpc_server_types::constants::gas_oracle::{CALL_STIPEND_GAS, ESTIMATE_GAS_ERROR_RATIO};
use reth_storage_api::StateProvider;
use revm::context_interface::{result::ExecutionResult, Transaction};
use tracing::trace;

impl<N, Rpc> EstimateCall for HlEthApi<N, Rpc>
where
    Self: Call,
    N: HlRpcNodeCore,
    EthApiError: FromEvmError<N::Evm> + From<StateOverrideError<ProviderError>>,
    Rpc: RpcConvert<
        Primitives = N::Primitives,
        Error = EthApiError,
        TxEnv = TxEnvFor<N::Evm>,
        Spec = SpecFor<N::Evm>,
    >,
{
    // Modified version that adds `apply_precompiles`; comments are stripped out.
    fn estimate_gas_with<S>(
        &self,
        mut evm_env: EvmEnvFor<Self::Evm>,
        mut request: RpcTxReq<<Self::RpcConvert as RpcConvert>::Network>,
        state: S,
        state_override: Option<StateOverride>,
    ) -> Result<U256, Self::Error>
    where
        S: StateProvider,
    {
        evm_env.cfg_env.disable_eip3607 = true;
        evm_env.cfg_env.disable_base_fee = true;

        request.as_mut().take_nonce();

        let tx_request_gas_limit = request.as_ref().gas_limit();
        let tx_request_gas_price = request.as_ref().gas_price();
        let max_gas_limit = evm_env
            .cfg_env
            .tx_gas_limit_cap
            .map_or(evm_env.block_env.gas_limit, |cap| cap.min(evm_env.block_env.gas_limit));

        let mut highest_gas_limit = tx_request_gas_limit
            .map(|mut tx_gas_limit| {
                if max_gas_limit < tx_gas_limit {
                    tx_gas_limit = max_gas_limit;
                }
                tx_gas_limit
            })
            .unwrap_or(max_gas_limit);

        let mut db = CacheDB::new(StateProviderDatabase::new(state));

        if let Some(state_override) = state_override {
            apply_state_overrides(state_override, &mut db).map_err(
                |err: StateOverrideError<ProviderError>| {
                    let eth_api_error: EthApiError = EthApiError::from(err);
                    Self::Error::from(eth_api_error)
                },
            )?;
        }

        let mut tx_env = self.create_txn_env(&evm_env, request, &mut db)?;

        let mut is_basic_transfer = false;
        if tx_env.input().is_empty() {
            if let TxKind::Call(to) = tx_env.kind() {
                if let Ok(code) = db.db.account_code(&to) {
                    is_basic_transfer = code.map(|code| code.is_empty()).unwrap_or(true);
                }
            }
        }

        if tx_env.gas_price() > 0 {
            highest_gas_limit =
                highest_gas_limit.min(self.caller_gas_allowance(&mut db, &evm_env, &tx_env)?);
        }

        tx_env.set_gas_limit(tx_env.gas_limit().min(highest_gas_limit));

        let block_number = evm_env.block_env().number;
        let hl_extras = self.get_hl_extras(block_number.to::<u64>().into())?;

        let mut evm = self.evm_config().evm_with_env(&mut db, evm_env);
        apply_precompiles(&mut evm, &hl_extras);

        if is_basic_transfer {
            let mut min_tx_env = tx_env.clone();
            min_tx_env.set_gas_limit(MIN_TRANSACTION_GAS);

            if let Ok(res) = evm.transact(min_tx_env).map_err(Self::Error::from_evm_err) {
                if res.result.is_success() {
                    return Ok(U256::from(MIN_TRANSACTION_GAS));
                }
            }
        }

        trace!(target: "rpc::eth::estimate", ?tx_env, gas_limit = tx_env.gas_limit(), is_basic_transfer, "Starting gas estimation");

        let mut res = match evm.transact(tx_env.clone()).map_err(Self::Error::from_evm_err) {
            Err(err)
                if err.is_gas_too_high() &&
                    (tx_request_gas_limit.is_some() || tx_request_gas_price.is_some()) =>
            {
                return Self::map_out_of_gas_err(&mut evm, tx_env, max_gas_limit);
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
            ExecutionResult::Success { gas_refunded, .. } => gas_refunded,
            ExecutionResult::Halt { reason, .. } => {
                return Err(Self::Error::from_evm_halt(reason, tx_env.gas_limit()));
            }
            ExecutionResult::Revert { output, .. } => {
                return if tx_request_gas_limit.is_some() || tx_request_gas_price.is_some() {
                    Self::map_out_of_gas_err(&mut evm, tx_env, max_gas_limit)
                } else {
                    Err(RpcInvalidTransactionError::Revert(RevertError::new(output)).into_eth_err())
                };
            }
        };

        highest_gas_limit = tx_env.gas_limit();

        let mut gas_used = res.result.gas_used();

        let mut lowest_gas_limit = gas_used.saturating_sub(1);

        let optimistic_gas_limit = (gas_used + gas_refund + CALL_STIPEND_GAS) * 64 / 63;
        if optimistic_gas_limit < highest_gas_limit {
            let mut optimistic_tx_env = tx_env.clone();
            optimistic_tx_env.set_gas_limit(optimistic_gas_limit);

            res = evm.transact(optimistic_tx_env).map_err(Self::Error::from_evm_err)?;

            gas_used = res.result.gas_used();

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
            if (highest_gas_limit - lowest_gas_limit) as f64 / (highest_gas_limit as f64) <
                ESTIMATE_GAS_ERROR_RATIO
            {
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
