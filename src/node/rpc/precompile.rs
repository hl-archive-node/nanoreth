use alloy_eips::BlockId;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee_core::{async_trait, RpcResult};
use reth_rpc_convert::RpcConvert;
use reth_rpc_eth_types::EthApiError;
use tracing::trace;

use crate::node::{
    rpc::{HlEthApi, HlRpcNodeCore},
    types::HlExtras,
};

/// A custom RPC trait for fetching block precompile data.
#[rpc(server, namespace = "eth")]
#[async_trait]
pub trait HlBlockPrecompileApi {
    /// Fetches precompile data for a given block.
    #[method(name = "blockPrecompileData")]
    async fn block_precompile_data(&self, block: BlockId) -> RpcResult<HlExtras>;
}

pub struct HlBlockPrecompileExt<N: HlRpcNodeCore, Rpc: RpcConvert> {
    eth_api: HlEthApi<N, Rpc>,
}

impl<N: HlRpcNodeCore, Rpc: RpcConvert> HlBlockPrecompileExt<N, Rpc> {
    /// Creates a new instance of the [`HlBlockPrecompileExt`].
    pub fn new(eth_api: HlEthApi<N, Rpc>) -> Self {
        Self { eth_api }
    }
}

#[async_trait]
impl<N, Rpc> HlBlockPrecompileApiServer for HlBlockPrecompileExt<N, Rpc>
where
    N: HlRpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
    async fn block_precompile_data(&self, block: BlockId) -> RpcResult<HlExtras> {
        trace!(target: "rpc::eth", ?block, "Serving eth_blockPrecompileData");
        let hl_extras = self.eth_api.get_hl_extras(block).map_err(EthApiError::from)?;
        Ok(hl_extras)
    }
}
