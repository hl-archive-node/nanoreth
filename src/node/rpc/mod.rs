use crate::{
    chainspec::HlChainSpec,
    node::{evm::apply_precompiles, types::HlExtras},
    HlBlock, HlPrimitives,
};
use alloy_eips::BlockId;
use alloy_evm::Evm;
use alloy_network::Ethereum;
use alloy_primitives::U256;
use reth::{
    api::{FullNodeTypes, HeaderTy, NodeTypes, PrimitivesTy},
    builder::{
        rpc::{EthApiBuilder, EthApiCtx},
        FullNodeComponents,
    },
    rpc::{
        eth::{core::EthApiInner, DevSigner, FullEthApiServer},
        server_types::eth::{
            receipt::EthReceiptConverter, EthApiError, EthStateCache, FeeHistoryCache,
            GasPriceOracle,
        },
    },
    tasks::{
        pool::{BlockingTaskGuard, BlockingTaskPool},
        TaskSpawner,
    },
};
use reth_evm::{ConfigureEvm, Database, EvmEnvFor, HaltReasonFor, InspectorFor, TxEnvFor};
use reth_primitives::NodePrimitives;
use reth_provider::{
    BlockReaderIdExt, ChainSpecProvider, ProviderError, ProviderHeader, ProviderTx,
};
use reth_rpc::RpcTypes;
use reth_rpc_eth_api::{
    helpers::{
        pending_block::BuildPendingEnv, spec::SignersForApi, AddDevSigners, EthApiSpec, EthFees,
        EthState, LoadFee, LoadPendingBlock, LoadState, SpawnBlocking, Trace,
    },
    EthApiTypes, FromEvmError, RpcConvert, RpcConverter, RpcNodeCore, RpcNodeCoreExt,
    SignableTxRequest,
};
use revm::context::result::ResultAndState;
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

#[derive(Clone)]
pub struct HlEthApi<N: HlRpcNodeCore, Rpc: RpcConvert> {
    /// Gateway to node's core components.
    pub(crate) inner: Arc<HlEthApiInner<N, Rpc>>,
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

    fn tx_resp_builder(&self) -> &Self::RpcConvert {
        self.inner.eth_api.tx_resp_builder()
    }
}

impl<N, Rpc> RpcNodeCore for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives>,
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
    type Transaction = ProviderTx<Self::Provider>;
    type Rpc = Rpc::Network;

    #[inline]
    fn starting_block(&self) -> U256 {
        self.inner.eth_api.starting_block()
    }

    #[inline]
    fn signers(&self) -> &SignersForApi<Self> {
        self.inner.eth_api.signers()
    }
}

impl<N, Rpc> SpawnBlocking for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
    #[inline]
    fn io_task_spawner(&self) -> impl TaskSpawner {
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

impl<N, Rpc> Trace for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
    fn inspect<DB, I>(
        &self,
        db: DB,
        evm_env: EvmEnvFor<Self::Evm>,
        tx_env: TxEnvFor<Self::Evm>,
        inspector: I,
    ) -> Result<ResultAndState<HaltReasonFor<Self::Evm>>, Self::Error>
    where
        DB: Database<Error = ProviderError>,
        I: InspectorFor<Self::Evm, DB>,
    {
        let block_number = evm_env.block_env().number;
        let hl_extras = self.get_hl_extras(block_number.to::<u64>().into())?;

        let mut evm = self.evm_config().evm_with_env_and_inspector(db, evm_env, inspector);
        apply_precompiles(&mut evm, &hl_extras);
        evm.transact(tx_env).map_err(Self::Error::from_evm_err)
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

impl<N, Rpc> AddDevSigners for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    Rpc: RpcConvert<
        Network: RpcTypes<TransactionRequest: SignableTxRequest<ProviderTx<N::Provider>>>,
    >,
{
    fn with_dev_accounts(&self) {
        *self.inner.eth_api.signers().write() = DevSigner::random_signers(20)
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
        > + AddDevSigners,
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
