use std::time::Duration;

use alloy_json_rpc::RpcObject;
use alloy_network::Ethereum;
use alloy_primitives::{Bytes, B256};
use alloy_rpc_types::TransactionRequest;
use jsonrpsee::{
    http_client::{HttpClient, HttpClientBuilder},
    proc_macros::rpc,
    types::{error::INTERNAL_ERROR_CODE, ErrorObject},
};
use jsonrpsee_core::{async_trait, client::ClientT, ClientError, RpcResult};
use reth::rpc::{result::internal_rpc_err, server_types::eth::EthApiError};
use reth_rpc_eth_api::RpcReceipt;

#[rpc(server, namespace = "eth")]
pub trait EthForwarderApi<R: RpcObject> {
    #[method(name = "sendRawTransaction")]
    async fn send_raw_transaction(&self, tx: Bytes) -> RpcResult<B256>;

    #[method(name = "eth_sendTransaction")]
    async fn send_transaction(&self, _tx: TransactionRequest) -> RpcResult<B256>;

    #[method(name = "eth_sendRawTransactionSync")]
    async fn send_raw_transaction_sync(&self, tx: Bytes) -> RpcResult<R>;
}

pub struct EthForwarderExt {
    client: HttpClient,
}

impl EthForwarderExt {
    pub fn new(upstream_rpc_url: String) -> Self {
        let client =
            HttpClientBuilder::default().build(upstream_rpc_url).expect("Failed to build client");

        Self { client }
    }

    fn from_client_error(e: ClientError, internal_error_prefix: &str) -> ErrorObject<'static> {
        match e {
            ClientError::Call(e) => e,
            _ => ErrorObject::owned(
                INTERNAL_ERROR_CODE,
                format!("{internal_error_prefix}: {e:?}"),
                Some(()),
            ),
        }
    }
}

#[async_trait]
impl EthForwarderApiServer<RpcReceipt<Ethereum>> for EthForwarderExt {
    async fn send_raw_transaction(&self, tx: Bytes) -> RpcResult<B256> {
        let txhash = self
            .client
            .clone()
            .request("eth_sendRawTransaction", vec![tx])
            .await
            .map_err(|e| Self::from_client_error(e, "Failed to send transaction"))?;
        Ok(txhash)
    }

    async fn send_transaction(&self, _tx: TransactionRequest) -> RpcResult<B256> {
        Err(internal_rpc_err("Unimplemented"))
    }

    async fn send_raw_transaction_sync(&self, tx: Bytes) -> RpcResult<RpcReceipt<Ethereum>> {
        let hash = self.send_raw_transaction(tx).await?;
        const TIMEOUT_DURATION: Duration = Duration::from_secs(30);
        const INTERVAL: Duration = Duration::from_secs(1);

        tokio::time::timeout(TIMEOUT_DURATION, async {
            loop {
                let receipt =
                    self.client.request("eth_getTransactionReceipt", vec![hash]).await.map_err(
                        |e| Self::from_client_error(e, "Failed to get transaction receipt"),
                    )?;
                if let Some(receipt) = receipt {
                    return Ok(receipt);
                }
                tokio::time::sleep(INTERVAL).await;
            }
        })
        .await
        .unwrap_or_else(|_elapsed| {
            Err(EthApiError::TransactionConfirmationTimeout { hash, duration: TIMEOUT_DURATION }
                .into())
        })
    }
}
