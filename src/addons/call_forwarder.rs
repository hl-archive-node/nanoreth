use alloy_eips::BlockId;
use alloy_json_rpc::RpcObject;
use alloy_primitives::{Bytes, U256};
use alloy_rpc_types_eth::{
    state::{EvmOverrides, StateOverride},
    BlockOverrides,
};
use jsonrpsee::{
    http_client::{HttpClient, HttpClientBuilder},
    proc_macros::rpc,
    rpc_params,
    types::{error::INTERNAL_ERROR_CODE, ErrorObject},
};
use jsonrpsee_core::{async_trait, client::ClientT, ClientError, RpcResult};
use reth_rpc::eth::EthApiTypes;
use reth_rpc_eth_api::{helpers::EthCall, RpcTxReq};

#[rpc(server, namespace = "eth")]
pub(crate) trait CallForwarderApi<TxReq: RpcObject> {
    /// Executes a new message call immediately without creating a transaction on the block chain.
    #[method(name = "call")]
    async fn call(
        &self,
        request: TxReq,
        block_id: Option<BlockId>,
        state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> RpcResult<Bytes>;

    /// Generates and returns an estimate of how much gas is necessary to allow the transaction to
    /// complete.
    #[method(name = "estimateGas")]
    async fn estimate_gas(
        &self,
        request: TxReq,
        block_id: Option<BlockId>,
        state_override: Option<StateOverride>,
    ) -> RpcResult<U256>;
}

pub struct CallForwarderExt<EthApi> {
    upstream_client: HttpClient,
    eth_api: EthApi,
}

impl<EthApi> CallForwarderExt<EthApi> {
    pub fn new(upstream_rpc_url: String, eth_api: EthApi) -> Self {
        let upstream_client =
            HttpClientBuilder::default().build(upstream_rpc_url).expect("Failed to build client");

        Self { upstream_client, eth_api }
    }
}

#[async_trait]
impl<EthApi> CallForwarderApiServer<RpcTxReq<<EthApi as EthApiTypes>::NetworkTypes>>
    for CallForwarderExt<EthApi>
where
    EthApi: EthCall + Send + Sync + 'static,
{
    async fn call(
        &self,
        request: RpcTxReq<<EthApi as EthApiTypes>::NetworkTypes>,
        block_id: Option<BlockId>,
        state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> RpcResult<Bytes> {
        let is_latest = block_id.as_ref().map(|b| b.is_latest()).unwrap_or(true);
        let result = if is_latest {
            self.upstream_client
                .request(
                    "eth_call",
                    rpc_params![request, block_id, state_overrides, block_overrides],
                )
                .await
                .map_err(|e| match e {
                    ClientError::Call(e) => e,
                    _ => ErrorObject::owned(
                        INTERNAL_ERROR_CODE,
                        format!("Failed to call: {e:?}"),
                        Some(()),
                    ),
                })?
        } else {
            EthCall::call(
                &self.eth_api,
                request,
                block_id,
                EvmOverrides::new(state_overrides, block_overrides),
            )
            .await
            .map_err(|e| {
                ErrorObject::owned(INTERNAL_ERROR_CODE, format!("Failed to call: {e:?}"), Some(()))
            })?
        };

        Ok(result)
    }

    async fn estimate_gas(
        &self,
        request: RpcTxReq<<EthApi as EthApiTypes>::NetworkTypes>,
        block_id: Option<BlockId>,
        state_override: Option<StateOverride>,
    ) -> RpcResult<U256> {
        let is_latest = block_id.as_ref().map(|b| b.is_latest()).unwrap_or(true);
        let result = if is_latest {
            self.upstream_client
                .request("eth_estimateGas", rpc_params![request, block_id, state_override])
                .await
                .map_err(|e| match e {
                    ClientError::Call(e) => e,
                    _ => ErrorObject::owned(
                        INTERNAL_ERROR_CODE,
                        format!("Failed to estimate gas: {e:?}"),
                        Some(()),
                    ),
                })?
        } else {
            EthCall::estimate_gas_at(
                &self.eth_api,
                request,
                block_id.unwrap_or_default(),
                state_override,
            )
            .await
            .map_err(|e| {
                ErrorObject::owned(
                    INTERNAL_ERROR_CODE,
                    format!("Failed to estimate gas: {e:?}"),
                    Some(()),
                )
            })?
        };

        Ok(result)
    }
}
