use core::fmt;

use super::{HlEthApi, HlRpcNodeCore};
use crate::{
    HlBlock,
    evm::transaction::HlTxEnvExt,
    node::evm::{
        apply_precompiles, apply_precompiles_with_forwarder,
        read_precompile_forwarder::read_precompile_forwarder,
    },
};
use alloy_consensus::transaction::TxHashRef;
use alloy_evm::{
    Evm, EvmFactory,
    block::BlockExecutorFactory,
    overrides::{OverrideBlockHashes, apply_block_overrides, apply_state_overrides},
};
use alloy_network::TransactionBuilder;
use alloy_primitives::B256;
use alloy_rpc_types_eth::state::EvmOverrides;
use reth::rpc::server_types::eth::EthApiError;
use reth_evm::{
    ConfigureEvm, Database, EvmEnvFor, HaltReasonFor, InspectorFor, SpecFor, TransactionEnv,
    TxEnvFor,
};
use reth_primitives::{NodePrimitives, Recovered};
use reth_provider::{ProviderError, ProviderTx};
use reth_rpc_convert::RpcTxReq;
use reth_rpc_eth_api::{FromEvmError, RpcConvert, RpcNodeCore, helpers::Call};
use revm::{
    Database as RevmDatabase, DatabaseCommit, context::result::ResultAndState,
    context_interface::Transaction,
};
use tracing::{trace, warn};

impl<N> HlRpcNodeCore for N where N: RpcNodeCore<Primitives: NodePrimitives<Block = HlBlock>> {}

impl<N, Rpc> Call for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore<
        Evm: ConfigureEvm<
            BlockExecutorFactory: BlockExecutorFactory<EvmFactory: EvmFactory<Tx: HlTxEnvExt>>,
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
    #[inline]
    fn call_gas_limit(&self) -> u64 {
        self.inner.eth_api.gas_cap()
    }

    #[inline]
    fn max_simulate_blocks(&self) -> u64 {
        self.inner.eth_api.max_simulate_blocks()
    }

    fn transact<DB>(
        &self,
        db: DB,
        evm_env: EvmEnvFor<Self::Evm>,
        tx_env: TxEnvFor<Self::Evm>,
    ) -> Result<ResultAndState<HaltReasonFor<Self::Evm>>, Self::Error>
    where
        DB: Database<Error = ProviderError> + fmt::Debug,
    {
        let block_number =
            tx_env.rpc_state_block_number().unwrap_or(evm_env.block_env().number.to());
        let (hl_extras, forwarder) = self.hl_call_precompiles(block_number)?;

        let mut evm = self.evm_config().evm_with_env(db, evm_env);
        apply_precompiles_with_forwarder(&mut evm, &hl_extras, forwarder);
        let res = evm.transact(tx_env).map_err(Self::Error::from_evm_err)?;

        Ok(res)
    }

    fn transact_with_inspector<DB, I>(
        &self,
        db: DB,
        evm_env: EvmEnvFor<Self::Evm>,
        tx_env: TxEnvFor<Self::Evm>,
        inspector: I,
    ) -> Result<ResultAndState<HaltReasonFor<Self::Evm>>, Self::Error>
    where
        DB: Database<Error = ProviderError> + fmt::Debug,
        I: InspectorFor<Self::Evm, DB>,
    {
        let block_number =
            tx_env.rpc_state_block_number().unwrap_or(evm_env.block_env().number.to());
        let (hl_extras, forwarder) = self.hl_call_precompiles(block_number)?;

        let mut evm = self.evm_config().evm_with_env_and_inspector(db, evm_env, inspector);
        apply_precompiles_with_forwarder(&mut evm, &hl_extras, forwarder);
        let res = evm.transact(tx_env).map_err(Self::Error::from_evm_err)?;

        Ok(res)
    }

    fn replay_transactions_until<'a, DB, I>(
        &self,
        db: &mut DB,
        evm_env: EvmEnvFor<Self::Evm>,
        transactions: I,
        target_tx_hash: B256,
    ) -> Result<usize, Self::Error>
    where
        DB: Database<Error = ProviderError> + DatabaseCommit + core::fmt::Debug,
        I: IntoIterator<Item = Recovered<&'a ProviderTx<Self::Provider>>>,
    {
        let block_number = evm_env.block_env().number;
        let hl_extras = self.get_hl_extras(block_number.to::<u64>().into())?;

        let mut evm = self.evm_config().evm_with_env(db, evm_env);
        apply_precompiles(&mut evm, &hl_extras);

        let mut index = 0;
        for tx in transactions {
            if *tx.tx_hash() == target_tx_hash {
                // reached the target transaction
                break;
            }

            let tx_env = self.evm_config().tx_env(tx);
            evm.transact_commit(tx_env).map_err(Self::Error::from_evm_err)?;
            index += 1;
        }
        Ok(index)
    }

    fn prepare_call_env<DB>(
        &self,
        mut evm_env: EvmEnvFor<Self::Evm>,
        mut request: RpcTxReq<<Self::RpcConvert as RpcConvert>::Network>,
        db: &mut DB,
        overrides: EvmOverrides,
    ) -> Result<(EvmEnvFor<Self::Evm>, TxEnvFor<Self::Evm>), Self::Error>
    where
        DB: RevmDatabase + DatabaseCommit + OverrideBlockHashes,
        EthApiError: From<<DB as RevmDatabase>::Error>,
    {
        let rpc_state_block_number = evm_env.block_env.number.to();
        let request_has_gas_limit = request.as_ref().gas_limit().is_some();

        if let Some(requested_gas) = request.as_ref().gas_limit() {
            let global_gas_cap = self.call_gas_limit();
            if global_gas_cap != 0 && global_gas_cap < requested_gas {
                warn!(target: "rpc::eth::call", ?request, ?global_gas_cap, "Capping gas limit to global gas cap");
                request.as_mut().set_gas_limit(global_gas_cap);
            }
        } else {
            request.as_mut().set_gas_limit(self.call_gas_limit());
        }

        evm_env.cfg_env.disable_block_gas_limit = true;
        evm_env.cfg_env.disable_eip3607 = true;
        evm_env.cfg_env.disable_base_fee = true;
        evm_env.cfg_env.tx_gas_limit_cap = Some(u64::MAX);
        request.as_mut().take_nonce();

        if let Some(block_overrides) = overrides.block {
            apply_block_overrides(*block_overrides, db, &mut evm_env.block_env);
        }
        if let Some(state_overrides) = overrides.state {
            apply_state_overrides(state_overrides, db)
                .map_err(EthApiError::from_state_overrides_err)?;
        }

        let mut tx_env = self.create_txn_env(&evm_env, request, &mut *db)?;
        if read_precompile_forwarder().is_some() {
            tx_env.set_rpc_state_block_number(rpc_state_block_number);
        }

        if tx_env.gas_price() == 0 {
            evm_env.block_env.basefee = 0;
        }

        if !request_has_gas_limit && tx_env.gas_price() > 0 {
            trace!(target: "rpc::eth::call", ?tx_env, "Applying gas limit cap with caller allowance");
            let cap = self.caller_gas_allowance(db, &evm_env, &tx_env)?;
            tx_env.set_gas_limit(cap.min(evm_env.block_env.gas_limit));
        }

        Ok((evm_env, tx_env))
    }
}
