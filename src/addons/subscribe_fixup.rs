use crate::addons::utils::{EthWrapper, new_headers_stream, pipe_from_stream};
use alloy_rpc_types::pubsub::{Params, SubscriptionKind};
use async_trait::async_trait;
use jsonrpsee::PendingSubscriptionSink;
use jsonrpsee_types::ErrorObject;
use reth::tasks::TaskExecutor;
use reth_rpc::EthPubSub;
use reth_rpc_convert::RpcTransaction;
use reth_rpc_eth_api::{EthApiTypes, EthPubSubApiServer};
use std::sync::Arc;

pub struct SubscribeFixup<Eth: EthWrapper> {
    pubsub: Arc<EthPubSub<Eth>>,
    provider: Arc<Eth::Provider>,
    subscription_task_spawner: TaskExecutor,
}

#[async_trait]
impl<Eth: EthWrapper> EthPubSubApiServer<RpcTransaction<Eth::NetworkTypes>> for SubscribeFixup<Eth>
where
    ErrorObject<'static>: From<<Eth as EthApiTypes>::Error>,
{
    async fn subscribe(
        &self,
        pending: PendingSubscriptionSink,
        kind: SubscriptionKind,
        params: Option<Params>,
    ) -> jsonrpsee::core::SubscriptionResult {
        let sink = pending.accept().await?;
        let (pubsub, provider) = (self.pubsub.clone(), self.provider.clone());
        self.subscription_task_spawner.spawn_task(async move {
            if kind == SubscriptionKind::NewHeads {
                let _ = pipe_from_stream(sink, new_headers_stream::<Eth>(&provider)).await;
            } else {
                let _ = pubsub.handle_accepted(sink, kind, params).await;
            }
        });
        Ok(())
    }
}

impl<Eth: EthWrapper> SubscribeFixup<Eth> {
    pub fn new(
        pubsub: Arc<EthPubSub<Eth>>,
        provider: Arc<Eth::Provider>,
        subscription_task_spawner: TaskExecutor,
    ) -> Self
    where
        Eth: EthWrapper,
        ErrorObject<'static>: From<Eth::Error>,
    {
        Self { pubsub, provider, subscription_task_spawner }
    }
}
