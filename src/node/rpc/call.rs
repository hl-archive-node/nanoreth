use core::fmt;

use super::{HlEthApi, HlExtrasProvider, HlRpcNodeCore, pinned_state_block};
use crate::{
    HlBlock,
    evm::{spec::HlSpecId, transaction::HlTxEnvExt},
    node::evm::{
        apply_precompiles_with_forwarder, mrb, read_precompile_forwarder::read_precompile_forwarder,
    },
};
use alloy_evm::{
    Evm, EvmFactory,
    block::BlockExecutorFactory,
    overrides::{OverrideBlockHashes, apply_block_overrides, apply_state_overrides},
};
use alloy_network::TransactionBuilder;
use alloy_rpc_types_eth::state::EvmOverrides;
use reth::rpc::server_types::eth::EthApiError;
use reth_evm::{
    ConfigureEvm, Database, EvmEnvFor, HaltReasonFor, InspectorFor, SpecFor, TransactionEnvMut,
    TxEnvFor, env::BlockEnvironment,
};
use reth_primitives_traits::NodePrimitives;
use reth_provider::ProviderError;
use reth_revm::db::bal::EvmDatabaseError;
use reth_rpc_convert::RpcTxReq;
use reth_rpc_eth_api::{FromEvmError, RpcConvert, RpcNodeCore, helpers::Call};
use revm::context::Block as _;
use revm::{
    Database as RevmDatabase, DatabaseCommit,
    context::result::ResultAndState,
    context_interface::{Cfg, Transaction},
};
use tracing::{debug, trace};

impl<N> HlRpcNodeCore for N where
    N: RpcNodeCore<Primitives: NodePrimitives<Block = HlBlock>, Provider: HlExtrasProvider>
{
}

impl<N, Rpc> Call for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore<
        Evm: ConfigureEvm<
            BlockExecutorFactory: BlockExecutorFactory<EvmFactory: EvmFactory<Tx: HlTxEnvExt>>,
        >,
    >,
    EthApiError: FromEvmError<N::Evm>,
    SpecFor<N::Evm>: From<HlSpecId>,
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
        let pinned = pinned_state_block(tx_env.rpc_state_block_number(), &evm_env);

        let mut evm = self.evm_config().evm_with_env(db, evm_env);
        if let Some(block_number) = pinned {
            let (hl_extras, forwarder) = self.hl_call_precompiles(block_number)?;
            apply_precompiles_with_forwarder(&mut evm, &hl_extras, forwarder);
        }
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
        let pinned = pinned_state_block(tx_env.rpc_state_block_number(), &evm_env);

        let mut evm = self.evm_config().evm_with_env_and_inspector(db, evm_env, inspector);
        if let Some(block_number) = pinned {
            let (hl_extras, forwarder) = self.hl_call_precompiles(block_number)?;
            apply_precompiles_with_forwarder(&mut evm, &hl_extras, forwarder);
        }
        let res = evm.transact(tx_env).map_err(Self::Error::from_evm_err)?;

        Ok(res)
    }

    /// Mirrors upstream reth's `prepare_call_env`, plus two HL additions: an overridden block
    /// number selects its precompile spec, and the tx env records the state block so read
    /// precompiles can be forwarded for it.
    ///
    /// Keep this in step with upstream: it is where the RPC gas cap and the EVM memory limit are
    /// applied, and an override that drops either lets a single call run unbounded.
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
        let rpc_state_block_number: u64 = evm_env.block_env.number().saturating_to();
        let request_has_gas_limit = request.as_ref().gas_limit().is_some();

        if let Some(requested_gas) = request.as_ref().gas_limit() {
            let global_gas_cap = self.call_gas_limit();
            if global_gas_cap != 0 && global_gas_cap < requested_gas {
                debug!(target: "rpc::eth::call", requested_gas, global_gas_cap, "Capping gas limit to global gas cap");
                request.as_mut().set_gas_limit(global_gas_cap);
            }
        } else {
            request.as_mut().set_gas_limit(self.call_gas_limit());
        }

        evm_env.cfg_env.disable_block_gas_limit = true;
        evm_env.cfg_env.disable_eip3607 = true;
        evm_env.cfg_env.disable_base_fee = true;
        if !evm_env.cfg_env.is_amsterdam_eip8037_enabled() {
            evm_env.cfg_env.tx_gas_limit_cap = Some(u64::MAX);
        }
        evm_env.cfg_env.disable_fee_charge = true;
        evm_env.cfg_env.memory_limit = self.evm_memory_limit();
        request.as_mut().take_nonce();

        if let Some(block_overrides) = overrides.block {
            apply_block_overrides(*block_overrides, db, evm_env.block_env.inner_mut());
            // Precompile prices follow the block number, so an overridden number picks its spec.
            evm_env.cfg_env.spec =
                mrb::spec_at(evm_env.cfg_env.chain_id, evm_env.block_env.number().saturating_to())
                    .into();
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
            evm_env.block_env.inner_mut().basefee = 0;
        }

        if !request_has_gas_limit && tx_env.gas_price() > 0 {
            trace!(target: "rpc::eth::call", ?tx_env, "Applying gas limit cap with caller allowance");
            let mut cap = self.caller_gas_allowance(db, &evm_env, &tx_env)?;
            // The allowance must not raise the already applied RPC gas cap.
            if self.call_gas_limit() != 0 {
                cap = cap.min(tx_env.gas_limit());
            }
            tx_env.set_gas_limit(cap.min(evm_env.block_env.gas_limit()));
        }

        Ok((evm_env, tx_env))
    }
}
