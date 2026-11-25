use crate::{
    HlBlock, HlPrimitives,
    chainspec::HlChainSpec,
    evm::transaction::HlTxEnvExt,
    node::{
        evm::{
            ScopedReadPrecompileForwarder, apply_precompiles_with_forwarder, config::HlEvmConfig,
            factory::RpcPrecompileSource, read_precompile_forwarder::read_precompile_forwarder,
        },
        types::HlExtras,
    },
};
use alloy_eips::BlockId;
use alloy_evm::Evm;
use alloy_network::Ethereum;
use alloy_primitives::U256;
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
use reth_evm::{ConfigureEvm, EvmEnvFor, HaltReasonFor, InspectorFor, IntoTxEnv, TxEnvFor};
use reth_primitives_traits::NodePrimitives;
use reth_primitives_traits::RecoveredBlock;
use reth_provider::{BlockReaderIdExt, ChainSpecProvider, ProviderError, ProviderHeader};
use reth_rpc::RpcTypes;
use reth_rpc_eth_api::{
    EthApiTypes, FromEvmError, RpcConvert, RpcConverter, RpcNodeCore, RpcNodeCoreExt,
    helpers::{
        EthApiSpec, EthFees, EthState, EthSubscriptions, GetBlockAccessList, LoadFee,
        LoadPendingBlock, LoadState, SpawnBlocking, Trace, pending_block::BuildPendingEnv,
    },
};
use reth_rpc_eth_types::{EthApiSettings, cache::db::StateCacheDb};
use reth_storage_api::{BlockNumReader, ProviderBlock};
use revm::context::result::ResultAndState;
use std::{cmp::Ordering, fmt, marker::PhantomData, sync::Arc};
use tokio::sync::Semaphore;

mod block;
mod call;
pub mod engine_api;
mod estimate;
pub mod precompile;
mod simulate;
mod transaction;

pub trait HlRpcNodeCore:
    RpcNodeCore<Primitives: NodePrimitives<Block = HlBlock>, Provider: HlExtrasProvider>
{
}

/// Gas limit of a HyperEVM big block.
///
/// Most HyperEVM blocks are small blocks with a lower gas limit. A transaction that needs more gas
/// goes into a big block, so a small block's gas limit is not an upper bound for a transaction.
const BIG_BLOCK_GAS_LIMIT: u64 = 30_000_000;

/// Returns the gas ceiling for calls and estimates against a block.
///
/// Used by `eth_estimateGas` and by priced calls without `gas` (`eth_call`, `eth_callMany`,
/// `debug_traceCall`, `trace_call` and related methods). Upstream reth uses the block gas limit.
/// On a small block that rejects transactions that are valid in a big block, so this never goes
/// below the big block gas limit, like hl-node. A non-zero RPC gas cap still applies.
pub(crate) fn rpc_gas_ceiling(block_gas_limit: u64, rpc_gas_cap: u64) -> u64 {
    let ceiling = block_gas_limit.max(BIG_BLOCK_GAS_LIMIT);
    if rpc_gas_cap == 0 { ceiling } else { ceiling.min(rpc_gas_cap) }
}

/// Container type `HlEthApi`
pub(crate) struct HlEthApiInner<N: HlRpcNodeCore, Rpc: RpcConvert> {
    /// Gateway to node's core components.
    pub(crate) eth_api: EthApiInner<N, Rpc>,
    /// The node's EVM config with read precompile installation turned on; see
    /// [`RpcPrecompileSource`]. Every RPC helper reaches the EVM through this one.
    pub(crate) evm_config: N::Evm,
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

    fn eth_api_settings(&self) -> &EthApiSettings {
        self.inner.eth_api.eth_api_settings()
    }

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
        &self.inner.evm_config
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
    N: HlRpcNodeCore<Primitives = HlPrimitives, Evm = HlEvmConfig>,
    Rpc: RpcConvert<Primitives = HlPrimitives, Error = EthApiError, Evm = HlEvmConfig>,
{
}

impl<N, Rpc> Trace for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore<Primitives = HlPrimitives, Evm = HlEvmConfig>,
    Rpc: RpcConvert<Primitives = HlPrimitives, Error = EthApiError, Evm = HlEvmConfig>,
{
    fn inspect<'a>(
        &self,
        db: &'a mut StateCacheDb,
        evm_env: EvmEnvFor<Self::Evm>,
        tx_env: impl IntoTxEnv<TxEnvFor<Self::Evm>>,
        inspector: impl InspectorFor<Self::Evm, &'a mut StateCacheDb>,
    ) -> Result<ResultAndState<HaltReasonFor<Self::Evm>>, Self::Error> {
        let tx_env: TxEnvFor<Self::Evm> = tx_env.into_tx_env();
        let pinned = pinned_state_block(tx_env.rpc_state_block_number(), &evm_env);

        // The factory installs read precompiles for the EVM's block; a call whose block number
        // was overridden reads them as of the state block instead.
        let mut evm = self.evm_config().evm_with_env_and_inspector(db, evm_env, inspector);
        if let Some(block_number) = pinned {
            let (hl_extras, forwarder) = self.hl_call_precompiles(block_number)?;
            apply_precompiles_with_forwarder(&mut evm, &hl_extras, forwarder);
        }
        evm.transact(tx_env).map_err(<EthApiError as FromEvmError<HlEvmConfig>>::from_evm_err)
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
}

/// The state block a call's read precompiles must come from, when it is not the EVM's own block.
///
/// `prepare_call_env` records the state block before applying block overrides, while the RPC EVM
/// factory has already installed precompiles for the (possibly overridden) EVM block.
pub(crate) fn pinned_state_block<B: revm::context::Block, S>(
    rpc_state_block_number: Option<u64>,
    evm_env: &reth_evm::EvmEnv<S, B>,
) -> Option<u64> {
    let evm_block: u64 = evm_env.block_env.number().saturating_to();
    rpc_state_block_number.filter(|block| *block != evm_block)
}

/// Provider capabilities the read precompile lookups need.
pub trait HlExtrasProvider: BlockReaderIdExt<Block = HlBlock> + BlockNumReader {}
impl<P> HlExtrasProvider for P where P: BlockReaderIdExt<Block = HlBlock> + BlockNumReader {}

pub(crate) fn hl_extras_at<P: HlExtrasProvider>(
    provider: &P,
    block: BlockId,
) -> Result<HlExtras, ProviderError> {
    Ok(provider
        .block_by_id(block)?
        .map(|block| HlExtras {
            read_precompile_calls: block.body.read_precompile_calls.clone(),
            highest_precompile_address: block.body.highest_precompile_address,
            system_tx_receipts_hash: None,
        })
        .unwrap_or_default())
}

/// Read precompile data to run a call at `block_number` with.
///
/// Alongside the block's recorded calls this returns the forwarder to resolve unrecorded
/// inputs with, but only from the chain head onwards: HyperCore state is not archived, so
/// forwarding a historical call would answer it with today's values.
pub(crate) fn hl_call_precompiles_at<P: HlExtrasProvider>(
    provider: &P,
    block_number: u64,
) -> Result<(HlExtras, Option<ScopedReadPrecompileForwarder>), ProviderError> {
    let Some(forwarder) = read_precompile_forwarder() else {
        return Ok((hl_extras_at(provider, block_number.into())?, None));
    };

    let chain_info = provider.chain_info()?;
    let best_block_number = chain_info.best_number;
    let forwarder = Some((forwarder, chain_info.best_hash));

    match block_number.cmp(&best_block_number) {
        Ordering::Less => Ok((hl_extras_at(provider, block_number.into())?, None)),
        Ordering::Equal => Ok((hl_extras_at(provider, block_number.into())?, forwarder)),
        // A block that has not been mined has no body to take the precompile address range
        // from, so borrow the head's. Without this the range falls back to `0x800..=0x80d`
        // and everything above it — `bbo` and up — is left uninstalled, so reading one looks
        // like a call to a plain account rather than a precompile.
        //
        // Its recorded calls are left out: those belong to the head block's own transactions,
        // and the forwarder answers the same inputs anyway.
        Ordering::Greater => {
            let head = hl_extras_at(provider, best_block_number.into())?;
            Ok((
                HlExtras {
                    read_precompile_calls: None,
                    highest_precompile_address: head.highest_precompile_address,
                    system_tx_receipts_hash: None,
                },
                forwarder,
            ))
        }
    }
}

/// Read precompile data for blocks simulated on top of `parent_block_number`.
///
/// A simulated block has no canonical body, so it must never borrow recorded calls from the
/// canonical block at the same height. It only inherits the parent's precompile address range.
/// Forwarding is available when the simulation starts at the head, where the upstream's
/// `latest` HyperCore state matches the local state being simulated.
pub(crate) fn hl_simulation_precompiles_at<P: HlExtrasProvider>(
    provider: &P,
    parent_block_number: u64,
) -> Result<(HlExtras, Option<ScopedReadPrecompileForwarder>), ProviderError> {
    let Some(forwarder) = read_precompile_forwarder() else {
        return Ok((HlExtras::default(), None));
    };

    let chain_info = provider.chain_info()?;
    let best_block_number = chain_info.best_number;
    let extras_block_number = parent_block_number.min(best_block_number);
    let parent = hl_extras_at(provider, extras_block_number.into())?;
    let forwarder =
        (parent_block_number >= best_block_number).then_some((forwarder, chain_info.best_hash));

    Ok((
        HlExtras {
            read_precompile_calls: None,
            highest_precompile_address: parent.highest_precompile_address,
            system_tx_receipts_hash: None,
        },
        forwarder,
    ))
}

/// Looks up read precompile data from the node's provider, for the RPC EVM factory.
#[derive(Debug)]
struct ProviderPrecompileSource<P> {
    provider: P,
}

impl<P> RpcPrecompileSource for ProviderPrecompileSource<P>
where
    P: HlExtrasProvider + Send + Sync + fmt::Debug,
{
    fn precompiles_at(
        &self,
        block_number: u64,
    ) -> Option<(HlExtras, Option<ScopedReadPrecompileForwarder>)> {
        hl_call_precompiles_at(&self.provider, block_number)
            .inspect_err(|err| {
                tracing::warn!(
                    target: "rpc::eth",
                    block_number,
                    %err,
                    "Failed to load read precompile data; read precompile calls will fail"
                )
            })
            .ok()
    }
}

impl<N, Rpc> HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
    fn get_hl_extras(&self, block: BlockId) -> Result<HlExtras, ProviderError> {
        hl_extras_at(self.provider(), block)
    }

    fn hl_call_precompiles(
        &self,
        block_number: u64,
    ) -> Result<(HlExtras, Option<ScopedReadPrecompileForwarder>), ProviderError> {
        hl_call_precompiles_at(self.provider(), block_number)
    }

    fn hl_simulation_precompiles(
        &self,
        parent_block_number: u64,
    ) -> Result<(HlExtras, Option<ScopedReadPrecompileForwarder>), ProviderError> {
        hl_simulation_precompiles_at(self.provider(), parent_block_number)
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
        + RpcNodeCore<Primitives = PrimitivesTy<N::Types>, Evm = HlEvmConfig>,
    HlEvmConfig: ConfigureEvm<NextBlockEnvCtx: BuildPendingEnv<HeaderTy<N::Types>>>,
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
        let evm_config = RpcNodeCore::evm_config(ctx.components).with_rpc_precompiles(Arc::new(
            ProviderPrecompileSource { provider: provider.clone() },
        ));
        let eth_api = ctx.eth_api_builder().with_rpc_converter(rpc_converter).build_inner();

        Ok(HlEthApi { inner: Arc::new(HlEthApiInner { eth_api, evm_config }) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_gas_ceiling_uses_big_block_gas_limit() {
        // Small block: 30M, not the 3M block gas limit.
        assert_eq!(rpc_gas_ceiling(3_000_000, 50_000_000), 30_000_000);
        // Big block.
        assert_eq!(rpc_gas_ceiling(30_000_000, 50_000_000), 30_000_000);
        // A block gas limit above 30M is kept.
        assert_eq!(rpc_gas_ceiling(36_000_000, 50_000_000), 36_000_000);
    }

    #[test]
    fn rpc_gas_ceiling_respects_rpc_gas_cap() {
        assert_eq!(rpc_gas_ceiling(3_000_000, 5_000_000), 5_000_000);
        // A cap of 0 means no cap, as in `prepare_call_env`.
        assert_eq!(rpc_gas_ceiling(3_000_000, 0), 30_000_000);
    }
}
