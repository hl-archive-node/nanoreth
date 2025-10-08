use core::fmt;

use super::{HlEthApi, HlRpcNodeCore};
use crate::{node::evm::apply_precompiles, HlBlock};
use alloy_evm::Evm;
use alloy_primitives::B256;
use reth::rpc::server_types::eth::EthApiError;
use reth_evm::{ConfigureEvm, Database, EvmEnvFor, HaltReasonFor, InspectorFor, SpecFor, TxEnvFor};
use reth_primitives::{NodePrimitives, Recovered};
use reth_primitives_traits::SignedTransaction;
use reth_provider::{ProviderError, ProviderTx};
use reth_rpc_eth_api::{
    helpers::{Call, EthCall},
    FromEvmError, RpcConvert, RpcNodeCore,
};
use revm::{context::result::ResultAndState, DatabaseCommit};

impl<N> HlRpcNodeCore for N where N: RpcNodeCore<Primitives: NodePrimitives<Block = HlBlock>> {}

impl<N, Rpc> EthCall for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<
        Primitives = N::Primitives,
        Error = EthApiError,
        TxEnv = TxEnvFor<N::Evm>,
        Spec = SpecFor<N::Evm>,
    >,
{
}

impl<N, Rpc> Call for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
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
        let block_number = evm_env.block_env().number;
        let hl_extras = self.get_hl_extras(block_number.to::<u64>().into())?;

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
        DB: Database<Error = ProviderError> + fmt::Debug,
        I: InspectorFor<Self::Evm, DB>,
    {
        let block_number = evm_env.block_env().number;
        let hl_extras = self.get_hl_extras(block_number.to::<u64>().into())?;

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
}
