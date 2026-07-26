use core::fmt;

use super::{HlEthApi, HlRpcNodeCore};
use crate::{HlBlock, node::evm::apply_precompiles};
use alloy_consensus::transaction::TxHashRef;
use alloy_evm::Evm;
use alloy_primitives::B256;
use reth::rpc::server_types::eth::EthApiError;
use reth_evm::{ConfigureEvm, Database, EvmEnvFor, HaltReasonFor, InspectorFor, TxEnvFor};
use reth_primitives_traits::{NodePrimitives, Recovered};
use reth_provider::{ProviderError, ProviderTx};
use reth_rpc_eth_api::{
    FromEvmError, RpcConvert, RpcNodeCore,
    helpers::{Call, EthCall},
};
use reth_revm::db::bal::EvmDatabaseError;
use revm::{DatabaseCommit, context::Block as _, context::result::ResultAndState};

impl<N> HlRpcNodeCore for N where N: RpcNodeCore<Primitives: NodePrimitives<Block = HlBlock>> {}

impl<N, Rpc> EthCall for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
{
}

impl<N, Rpc> Call for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
{
    #[inline]
    fn call_gas_limit(&self) -> u64 {
        self.inner.eth_api.gas_cap()
    }

    #[inline]
    fn max_simulate_blocks(&self) -> u64 {
        self.inner.eth_api.max_simulate_blocks()
    }

    #[inline]
    fn compute_state_root_for_eth_simulate(&self) -> bool {
        self.inner.eth_api.compute_state_root_for_eth_simulate()
    }

    #[inline]
    fn evm_memory_limit(&self) -> u64 {
        self.inner.eth_api.evm_memory_limit()
    }

    fn transact<DB>(
        &self,
        db: DB,
        evm_env: EvmEnvFor<Self::Evm>,
        tx_env: TxEnvFor<Self::Evm>,
    ) -> Result<ResultAndState<HaltReasonFor<Self::Evm>>, Self::Error>
    where
        DB: Database<Error = EvmDatabaseError<ProviderError>> + fmt::Debug,
    {
        let block_number: u64 = evm_env.block_env.number().saturating_to();
        let hl_extras = self.get_hl_extras(block_number.into())?;

        let mut evm = self.evm_config().evm_with_env(db, evm_env);
        apply_precompiles(&mut evm, &hl_extras);
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
        DB: Database<Error = EvmDatabaseError<ProviderError>> + fmt::Debug,
        I: InspectorFor<Self::Evm, DB>,
    {
        let block_number: u64 = evm_env.block_env.number().saturating_to();
        let hl_extras = self.get_hl_extras(block_number.into())?;

        let mut evm = self.evm_config().evm_with_env_and_inspector(db, evm_env, inspector);
        apply_precompiles(&mut evm, &hl_extras);
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
        DB: Database<Error = EvmDatabaseError<ProviderError>> + DatabaseCommit + core::fmt::Debug,
        I: IntoIterator<Item = Recovered<&'a ProviderTx<Self::Provider>>>,
    {
        let block_number: u64 = evm_env.block_env.number().saturating_to();
        let hl_extras = self.get_hl_extras(block_number.into())?;

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
}
