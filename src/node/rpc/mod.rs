use crate::{
    HlBlock, HlPrimitives,
    chainspec::HlChainSpec,
    node::{evm::apply_precompiles, types::HlExtras},
};
use alloy_consensus::{BlockHeader, transaction::TxHashRef};
use alloy_eips::BlockId;
use alloy_evm::{Evm, EvmFactory};
use alloy_network::Ethereum;
use alloy_primitives::U256;
use alloy_rpc_types_eth::TransactionInfo;
use reth::{
    api::{FullNodeTypes, HeaderTy, NodeTypes, PrimitivesTy},
    builder::{
        FullNodeComponents,
        rpc::{EthApiBuilder, EthApiCtx},
    },
    rpc::{
        eth::{FullEthApiServer, core::EthApiInner},
        server_types::eth::{
            EthApiError, EthStateCache, FeeHistoryCache, GasPriceOracle,
            receipt::EthReceiptConverter,
        },
    },
    tasks::{
        Runtime,
        pool::{BlockingTaskGuard, BlockingTaskPool},
    },
};
use reth_evm::{
    ConfigureEvm, Database, EvmEnvFor, EvmFor, HaltReasonFor, InspectorFor, TxEnvFor,
    tracing::{TracingCtx, TxTracer},
};
use reth_primitives_traits::NodePrimitives;
use reth_primitives_traits::{BlockBody, Recovered, RecoveredBlock};
use reth_provider::{
    BlockReaderIdExt, ChainSpecProvider, ProviderError, ProviderHeader, ProviderTx,
};
use reth_revm::db::bal::EvmDatabaseError;
use reth_rpc::RpcTypes;
use reth_rpc_eth_api::{
    EthApiTypes, FromEvmError, RpcConvert, RpcConverter, RpcNodeCore, RpcNodeCoreExt,
    helpers::{
        Call, EthApiSpec, EthFees, EthState, EthSubscriptions, GetBlockAccessList, LoadBlock,
        LoadFee, LoadPendingBlock, LoadState, SpawnBlocking, Trace, pending_block::BuildPendingEnv,
    },
};
use reth_rpc_eth_types::cache::db::StateCacheDb;
use reth_storage_api::ProviderBlock;
use revm::{context::Block as _, context::result::ResultAndState};
use tokio::sync::Semaphore;
use std::{fmt, marker::PhantomData, sync::Arc};

mod block;
mod call;
pub mod engine_api;
mod estimate;
pub mod precompile;
mod transaction;

pub trait HlRpcNodeCore: RpcNodeCore<Primitives: NodePrimitives<Block = HlBlock>> {}

/// Container type `HlEthApi`
pub(crate) struct HlEthApiInner<N: HlRpcNodeCore, Rpc: RpcConvert> {
    /// Gateway to node's core components.
    pub(crate) eth_api: EthApiInner<N, Rpc>,
}

type HlRpcConvert<N, NetworkT> =
    RpcConverter<NetworkT, <N as FullNodeComponents>::Evm, EthReceiptConverter<HlChainSpec>>;

pub struct HlEthApi<N: HlRpcNodeCore, Rpc: RpcConvert> {
    /// Gateway to node's core components.
    pub(crate) inner: Arc<HlEthApiInner<N, Rpc>>,
}

impl<N: HlRpcNodeCore, Rpc: RpcConvert> Clone for HlEthApi<N, Rpc> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

impl<N, Rpc> fmt::Debug for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HlEthApi").finish_non_exhaustive()
    }
}

impl<N, Rpc> EthApiTypes for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
    type Error = EthApiError;
    type NetworkTypes = Rpc::Network;
    type RpcConvert = Rpc;

    fn converter(&self) -> &Self::RpcConvert {
        self.inner.eth_api.converter()
    }
}

impl<N, Rpc> RpcNodeCore for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
    type Primitives = N::Primitives;
    type Provider = N::Provider;
    type Pool = N::Pool;
    type Evm = N::Evm;
    type Network = N::Network;

    #[inline]
    fn pool(&self) -> &Self::Pool {
        self.inner.eth_api.pool()
    }

    #[inline]
    fn evm_config(&self) -> &Self::Evm {
        self.inner.eth_api.evm_config()
    }

    #[inline]
    fn network(&self) -> &Self::Network {
        self.inner.eth_api.network()
    }

    #[inline]
    fn provider(&self) -> &Self::Provider {
        self.inner.eth_api.provider()
    }
}

impl<N, Rpc> RpcNodeCoreExt for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
    #[inline]
    fn cache(&self) -> &EthStateCache<N::Primitives> {
        self.inner.eth_api.cache()
    }
}

impl<N, Rpc> EthApiSpec for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
    #[inline]
    fn starting_block(&self) -> U256 {
        self.inner.eth_api.starting_block()
    }
}

impl<N, Rpc> SpawnBlocking for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
    #[inline]
    fn io_task_spawner(&self) -> &Runtime {
        self.inner.eth_api.task_spawner()
    }

    #[inline]
    fn tracing_task_pool(&self) -> &BlockingTaskPool {
        self.inner.eth_api.blocking_task_pool()
    }

    #[inline]
    fn tracing_task_guard(&self) -> &BlockingTaskGuard {
        self.inner.eth_api.blocking_task_guard()
    }

    #[inline]
    fn blocking_io_task_guard(&self) -> &Arc<Semaphore> {
        self.inner.eth_api.blocking_io_request_semaphore()
    }
}

impl<N, Rpc> LoadFee for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
    #[inline]
    fn gas_oracle(&self) -> &GasPriceOracle<Self::Provider> {
        self.inner.eth_api.gas_oracle()
    }

    #[inline]
    fn fee_history_cache(&self) -> &FeeHistoryCache<ProviderHeader<N::Provider>> {
        self.inner.eth_api.fee_history_cache()
    }
}

impl<N, Rpc> LoadState for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
    Self: LoadPendingBlock,
{
}

impl<N, Rpc> EthState for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
    Self: LoadPendingBlock,
{
    #[inline]
    fn max_proof_window(&self) -> u64 {
        self.inner.eth_api.eth_proof_window()
    }
}

impl<N, Rpc> EthFees for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
}

impl<N, Rpc> EthSubscriptions for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
}

impl<N, Rpc> GetBlockAccessList for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
{
}

impl<N, Rpc> Trace for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
    Self: Call,
{
    fn inspect<DB, I>(
        &self,
        db: DB,
        evm_env: EvmEnvFor<Self::Evm>,
        tx_env: TxEnvFor<Self::Evm>,
        inspector: I,
    ) -> Result<ResultAndState<HaltReasonFor<Self::Evm>>, Self::Error>
    where
        DB: Database<Error = EvmDatabaseError<ProviderError>>,
        I: InspectorFor<Self::Evm, DB>,
    {
        let block_number: u64 = evm_env.block_env.number().saturating_to();
        let hl_extras = self.get_hl_extras(block_number.into())?;

        let mut evm = self.evm_config().evm_with_env_and_inspector(db, evm_env, inspector);
        apply_precompiles(&mut evm, &hl_extras);
        evm.transact(tx_env).map_err(Self::Error::from_evm_err)
    }

    fn apply_pre_execution_changes(
        &self,
        _block: &RecoveredBlock<ProviderBlock<Self::Provider>>,
        _db: &mut StateCacheDb,
    ) -> Result<(), Self::Error> {
        // HL dynamic precompiles are EVM-local, so they must be installed on the actual
        // execution EVM rather than through this DB/env-only hook.
        Ok(())
    }

    async fn trace_block_until_with_inspector<Setup, Insp, F, R>(
        &self,
        block_id: BlockId,
        block: Option<Arc<RecoveredBlock<ProviderBlock<Self::Provider>>>>,
        highest_index: Option<u64>,
        mut inspector_setup: Setup,
        f: F,
    ) -> Result<Option<Vec<R>>, Self::Error>
    where
        Self: LoadBlock,
        F: Fn(
                TransactionInfo,
                TracingCtx<
                    '_,
                    Recovered<&ProviderTx<Self::Provider>>,
                    EvmFor<Self::Evm, &mut StateCacheDb, Insp>,
                >,
            ) -> Result<R, Self::Error>
            + Send
            + 'static,
        Setup: FnMut() -> Insp + Send + 'static,
        Insp: Clone + for<'a> InspectorFor<Self::Evm, &'a mut StateCacheDb>,
        R: Send + 'static,
    {
        let block = if block.is_some() { block } else { self.recovered_block(block_id).await? };

        let Some(block) = block else { return Ok(None) };
        let evm_env = self.evm_env_for_header(block.sealed_block().sealed_header())?;

        if block.body().transactions().is_empty() {
            return Ok(Some(Vec::new()));
        }

        self.spawn_with_state_at_block(block.parent_hash(), move |this, mut db| {
            let block_hash = block.hash();
            let block_number: u64 = evm_env.block_env.number().saturating_to();
            let block_timestamp = evm_env.block_env.timestamp().saturating_to();
            let base_fee = evm_env.block_env.basefee();
            let hl_extras = this.get_hl_extras(block_number.into()).map_err(
                |err: ProviderError| <EthApiError as From<ProviderError>>::from(err),
            )?;

            let max_transactions = highest_index.map_or_else(
                || block.body().transaction_count(),
                |highest| highest as usize + 1,
            );

            let mut idx = 0;
            let mut evm = this.evm_config().evm_factory().create_evm_with_inspector(
                &mut db,
                evm_env,
                inspector_setup(),
            );
            apply_precompiles(&mut evm, &hl_extras);
            let mut tracer = TxTracer::new(evm);

            let results = tracer
                .try_trace_many(block.transactions_recovered().take(max_transactions), |ctx| {
                    let tx_info = TransactionInfo {
                        hash: Some(*ctx.tx.tx_hash()),
                        index: Some(idx),
                        block_hash: Some(block_hash),
                        block_number: Some(block_number),
                        block_timestamp: Some(block_timestamp),
                        base_fee: Some(base_fee),
                    };
                    idx += 1;

                    f(tx_info, ctx)
                })
                .collect::<Result<_, _>>()?;

            Ok(Some(results))
        })
        .await
    }
}

impl<N, Rpc> HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
    fn get_hl_extras(&self, block: BlockId) -> Result<HlExtras, ProviderError> {
        Ok(self
            .provider()
            .block_by_id(block)?
            .map(|block| HlExtras {
                read_precompile_calls: block.body.read_precompile_calls.clone(),
                highest_precompile_address: block.body.highest_precompile_address,
            })
            .unwrap_or_default())
    }
}

/// Builds [`HlEthApi`] for HL.
#[derive(Debug)]
#[non_exhaustive]
pub struct HlEthApiBuilder<NetworkT = Ethereum> {
    /// Marker for network types.
    pub(crate) _nt: PhantomData<NetworkT>,
}

impl<NetworkT> Default for HlEthApiBuilder<NetworkT> {
    fn default() -> Self {
        Self { _nt: PhantomData }
    }
}

impl<N, NetworkT> EthApiBuilder<N> for HlEthApiBuilder<NetworkT>
where
    N: FullNodeComponents<Types: NodeTypes<ChainSpec = HlChainSpec, Primitives = HlPrimitives>>
        + RpcNodeCore<
            Primitives = PrimitivesTy<N::Types>,
            Evm: ConfigureEvm<NextBlockEnvCtx: BuildPendingEnv<HeaderTy<N::Types>>>,
        >,
    NetworkT: RpcTypes,
    HlRpcConvert<N, NetworkT>: RpcConvert<Network = NetworkT, Primitives = PrimitivesTy<N::Types>>,
    HlEthApi<N, HlRpcConvert<N, NetworkT>>: FullEthApiServer<
            Provider = <N as FullNodeTypes>::Provider,
            Pool = <N as FullNodeComponents>::Pool,
        >,
{
    type EthApi = HlEthApi<N, HlRpcConvert<N, NetworkT>>;

    async fn build_eth_api(self, ctx: EthApiCtx<'_, N>) -> eyre::Result<Self::EthApi> {
        let provider = FullNodeComponents::provider(ctx.components);
        let rpc_converter =
            RpcConverter::new(EthReceiptConverter::<HlChainSpec>::new(provider.chain_spec()));
        let eth_api = ctx.eth_api_builder().with_rpc_converter(rpc_converter).build_inner();

        Ok(HlEthApi { inner: Arc::new(HlEthApiInner { eth_api }) })
    }
}
