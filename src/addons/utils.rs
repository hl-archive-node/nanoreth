use std::sync::Arc;

use crate::{HlBlock, HlPrimitives};
use alloy_primitives::U256;
use alloy_rpc_types::Header;
use futures::StreamExt;
use jsonrpsee::{SubscriptionMessage, SubscriptionSink};
use jsonrpsee_types::ErrorObject;
use reth_primitives_traits::{SealedHeader, TxTy};
use reth_provider::{BlockReader, CanonStateSubscriptions};
use reth_rpc::{RpcTypes, eth::pubsub::SubscriptionSerializeError};
use reth_rpc_convert::{RpcBlock, RpcHeader, RpcReceipt, RpcTransaction, RpcTxReq};
use reth_rpc_eth_api::{
    EthApiServer, FullEthApiTypes, RpcNodeCoreExt,
    helpers::{EthBlocks, EthSubscriptions, EthTransactions, LoadReceipt},
};
use serde::Serialize;
use tokio_stream::Stream;

pub trait EthWrapper:
    EthApiServer<
        RpcTxReq<Self::NetworkTypes>,
        RpcTransaction<Self::NetworkTypes>,
        RpcBlock<Self::NetworkTypes>,
        RpcReceipt<Self::NetworkTypes>,
        RpcHeader<Self::NetworkTypes>,
        TxTy<Self::Primitives>,
    > + FullEthApiTypes<
        Primitives = HlPrimitives,
        NetworkTypes: RpcTypes<TransactionResponse = alloy_rpc_types_eth::Transaction>,
    > + RpcNodeCoreExt<Provider: BlockReader<Block = HlBlock>>
    + EthBlocks
    + EthTransactions
    + EthSubscriptions
    + LoadReceipt
    + 'static
{
}

impl<T> EthWrapper for T where
    T: EthApiServer<
            RpcTxReq<Self::NetworkTypes>,
            RpcTransaction<Self::NetworkTypes>,
            RpcBlock<Self::NetworkTypes>,
            RpcReceipt<Self::NetworkTypes>,
            RpcHeader<Self::NetworkTypes>,
            TxTy<Self::Primitives>,
        > + FullEthApiTypes<
            Primitives = HlPrimitives,
            NetworkTypes: RpcTypes<TransactionResponse = alloy_rpc_types_eth::Transaction>,
        > + RpcNodeCoreExt<Provider: BlockReader<Block = HlBlock>>
        + EthBlocks
        + EthTransactions
        + EthSubscriptions
        + LoadReceipt
        + 'static
{
}

pub(super) async fn pipe_from_stream<T: Serialize, St: Stream<Item = T> + Unpin>(
    sink: SubscriptionSink,
    mut stream: St,
) -> Result<(), ErrorObject<'static>> {
    loop {
        tokio::select! {
            _ = sink.closed() => break Ok(()),
            maybe_item = stream.next() => {
                let Some(item) = maybe_item else { break Ok(()) };
                let msg = SubscriptionMessage::new(sink.method_name(), sink.subscription_id(), &item)
                    .map_err(SubscriptionSerializeError::from)?;
                if sink.send(msg).await.is_err() { break Ok(()); }
            }
        }
    }
}

pub(super) fn new_headers_stream<Eth: EthWrapper>(
    provider: &Arc<Eth::Provider>,
) -> impl Stream<Item = Header<alloy_consensus::Header>> {
    provider.canonical_state_stream().flat_map(|new_chain| {
        let headers = new_chain
            .committed()
            .blocks_iter()
            .map(|block| {
                Header::from_consensus(
                    SealedHeader::new(block.header().inner.clone(), block.hash()).into(),
                    None,
                    Some(U256::from(block.rlp_length())),
                )
            })
            .collect::<Vec<_>>();
        futures::stream::iter(headers)
    })
}
