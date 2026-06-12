//! Overrides for RPC methods to post-filter system transactions and logs.
//!
//! System transactions are always at the beginning of the block,
//! so we can use the transaction index to determine if the log is from a system transaction,
//! and if it is, we can exclude it.
//!
//! For non-system transactions, we can just return the log as is, and the client will
//! adjust the transaction index accordingly.

use alloy_consensus::{
    BlockHeader, TxReceipt,
    transaction::{TransactionMeta, TxHashRef},
};
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_json_rpc::RpcObject;
use alloy_primitives::{B256, U256};
use alloy_rpc_types::{
    BlockTransactions, Filter, FilterChanges, FilterId, Log, TransactionInfo,
    pubsub::{Params, SubscriptionKind},
};
use jsonrpsee::{PendingSubscriptionSink, proc_macros::rpc};
use jsonrpsee_core::{RpcResult, async_trait};
use jsonrpsee_types::{ErrorObject, error::INTERNAL_ERROR_CODE};
use reth::{api::FullNodeComponents, builder::rpc::RpcContext, tasks::TaskSpawner};
use reth_primitives_traits::SignedTransaction;
use reth_provider::{BlockIdReader, BlockReader, BlockReaderIdExt, ReceiptProvider};
use reth_rpc::{EthFilter, EthPubSub};
use reth_rpc_eth_api::{
    EthApiTypes, EthFilterApiServer, RpcBlock, RpcConvert, RpcReceipt, RpcTransaction,
    helpers::{EthBlocks, EthTransactions}, transaction::ConvertReceiptInput,
};
use reth_rpc_eth_types::EthApiError;
use serde::{Deserialize, Serialize};
use std::{marker::PhantomData, sync::Arc};
use tokio_stream::StreamExt;
use tracing::{Instrument, trace};

use crate::addons::utils::{EthWrapper, new_headers_stream, pipe_from_stream};
use http::Extensions;
use super::layer::is_hl_compliant;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockReceiptsWithSystemTx<R> {
    pub receipts: Vec<R>,
    pub system_tx_receipts: Vec<R>,
}

#[rpc(server, namespace = "eth")]
#[async_trait]
pub trait EthSystemTransactionApi<T: RpcObject, R: RpcObject> {
    #[method(name = "getEvmSystemTxsByBlockHash")]
    async fn get_evm_system_txs_by_block_hash(&self, hash: B256) -> RpcResult<Option<Vec<T>>>;

    #[method(name = "getEvmSystemTxsByBlockNumber")]
    async fn get_evm_system_txs_by_block_number(
        &self,
        block_id: Option<BlockId>,
    ) -> RpcResult<Option<Vec<T>>>;

    #[method(name = "getEvmSystemTxsReceiptsByBlockHash")]
    async fn get_evm_system_txs_receipts_by_block_hash(
        &self,
        hash: B256,
    ) -> RpcResult<Option<Vec<R>>>;

    #[method(name = "getEvmSystemTxsReceiptsByBlockNumber")]
    async fn get_evm_system_txs_receipts_by_block_number(
        &self,
        block_id: Option<BlockId>,
    ) -> RpcResult<Option<Vec<R>>>;
}

pub struct HlSystemTransactionExt<Eth: EthWrapper> {
    eth_api: Eth,
    _marker: PhantomData<Eth>,
}

impl<Eth: EthWrapper> HlSystemTransactionExt<Eth> {
    pub fn new(eth_api: Eth) -> Self {
        Self { eth_api, _marker: PhantomData }
    }

    async fn get_system_txs_by_block_id(
        &self,
        block_id: BlockId,
    ) -> RpcResult<Option<Vec<RpcTransaction<Eth::NetworkTypes>>>>
    where
        jsonrpsee_types::ErrorObject<'static>: From<<Eth as EthApiTypes>::Error>,
    {
        if let Some(block) = self.eth_api.recovered_block(block_id).await? {
            let block_hash = block.hash();
            let block_number = block.number();
            let base_fee_per_gas = block.base_fee_per_gas();
            let system_txs = block
                .transactions_with_sender()
                .enumerate()
                .filter_map(|(index, (signer, tx))| {
                    if tx.is_system_transaction() {
                        let tx_info = TransactionInfo {
                            hash: Some(*tx.tx_hash()),
                            block_hash: Some(block_hash),
                            block_number: Some(block_number),
                            base_fee: base_fee_per_gas,
                            index: Some(index as u64),
                        };
                        self.eth_api
                            .tx_resp_builder()
                            .fill(tx.clone().with_signer(*signer), tx_info)
                            .ok()
                    } else {
                        None
                    }
                })
                .collect();
            Ok(Some(system_txs))
        } else {
            Ok(None)
        }
    }

    async fn get_system_txs_receipts_by_block_id(
        &self,
        block_id: BlockId,
    ) -> RpcResult<Option<Vec<RpcReceipt<Eth::NetworkTypes>>>>
    where
        jsonrpsee_types::ErrorObject<'static>: From<<Eth as EthApiTypes>::Error>,
    {
        if let Some((block, receipts)) =
            EthBlocks::load_block_and_receipts(&self.eth_api, block_id).await?
        {
            let block_number = block.number;
            let base_fee = block.base_fee_per_gas;
            let block_hash = block.hash();
            let excess_blob_gas = block.excess_blob_gas;
            let timestamp = block.timestamp;
            let mut gas_used = 0;
            let mut next_log_index = 0;

            let mut inputs = Vec::new();
            for (idx, (tx, receipt)) in
                block.transactions_recovered().zip(receipts.iter()).enumerate()
            {
                if receipt.cumulative_gas_used() != 0 {
                    break;
                }

                let meta = TransactionMeta {
                    tx_hash: *tx.tx_hash(),
                    index: idx as u64,
                    block_hash,
                    block_number,
                    base_fee,
                    excess_blob_gas,
                    timestamp,
                };

                let input = ConvertReceiptInput {
                    receipt: receipt.clone(),
                    tx,
                    gas_used: receipt.cumulative_gas_used() - gas_used,
                    next_log_index,
                    meta,
                };

                gas_used = receipt.cumulative_gas_used();
                next_log_index += receipt.logs().len();

                inputs.push(input);
            }

            let receipts = self.eth_api.tx_resp_builder().convert_receipts(inputs)?;
            Ok(Some(receipts))
        } else {
            Ok(None)
        }
    }
}

#[async_trait]
impl<Eth: EthWrapper>
    EthSystemTransactionApiServer<RpcTransaction<Eth::NetworkTypes>, RpcReceipt<Eth::NetworkTypes>>
    for HlSystemTransactionExt<Eth>
where
    jsonrpsee_types::ErrorObject<'static>: From<<Eth as EthApiTypes>::Error>,
{
    /// Returns the system transactions for a given block hash.
    /// Semi-compliance with the `eth_getSystemTxsByBlockHash` RPC method introduced by hl-node.
    /// https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/hyperevm/json-rpc
    ///
    /// NOTE: Method name differs from hl-node because we retrieve transaction data from EVM
    /// (signature recovery for 'from' address, EVM hash calculation) rather than HyperCore.
    async fn get_evm_system_txs_by_block_hash(
        &self,
        hash: B256,
    ) -> RpcResult<Option<Vec<RpcTransaction<Eth::NetworkTypes>>>> {
        trace!(target: "rpc::eth", ?hash, "Serving eth_getEvmSystemTxsByBlockHash");
        match self.get_system_txs_by_block_id(BlockId::Hash(hash.into())).await {
            Ok(txs) => Ok(txs),
            // hl-node returns none if the block is not found
            Err(_) => Ok(None),
        }
    }

    /// Returns the system transactions for a given block number, or the latest block if no block
    /// number is provided. Semi-compliance with the `eth_getSystemTxsByBlockNumber` RPC method
    /// introduced by hl-node. https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/hyperevm/json-rpc
    ///
    /// NOTE: Method name differs from hl-node because we retrieve transaction data from EVM
    /// (signature recovery for 'from' address, EVM hash calculation) rather than HyperCore.
    async fn get_evm_system_txs_by_block_number(
        &self,
        id: Option<BlockId>,
    ) -> RpcResult<Option<Vec<RpcTransaction<Eth::NetworkTypes>>>> {
        trace!(target: "rpc::eth", ?id, "Serving eth_getEvmSystemTxsByBlockNumber");
        match self.get_system_txs_by_block_id(id.unwrap_or_default()).await? {
            Some(txs) => Ok(Some(txs)),
            None => {
                // hl-node returns an error if the block is not found
                Err(ErrorObject::owned(
                    INTERNAL_ERROR_CODE,
                    format!("invalid block height: {id:?}"),
                    Some(()),
                ))
            }
        }
    }

    /// Returns the receipts for the system transactions for a given block hash.
    async fn get_evm_system_txs_receipts_by_block_hash(
        &self,
        hash: B256,
    ) -> RpcResult<Option<Vec<RpcReceipt<Eth::NetworkTypes>>>> {
        trace!(target: "rpc::eth", ?hash, "Serving eth_getEvmSystemTxsReceiptsByBlockHash");
        match self.get_system_txs_receipts_by_block_id(BlockId::Hash(hash.into())).await {
            Ok(receipts) => Ok(receipts),
            // hl-node returns none if the block is not found
            Err(_) => Ok(None),
        }
    }

    /// Returns the receipts for the system transactions for a given block number, or the latest
    /// block if no block
    async fn get_evm_system_txs_receipts_by_block_number(
        &self,
        block_id: Option<BlockId>,
    ) -> RpcResult<Option<Vec<RpcReceipt<Eth::NetworkTypes>>>> {
        trace!(target: "rpc::eth", ?block_id, "Serving eth_getEvmSystemTxsReceiptsByBlockNumber");
        match self.get_system_txs_receipts_by_block_id(block_id.unwrap_or_default()).await? {
            Some(receipts) => Ok(Some(receipts)),
            None => Err(ErrorObject::owned(
                INTERNAL_ERROR_CODE,
                format!("invalid block height: {block_id:?}"),
                Some(()),
            )),
        }
    }
}

pub struct HlNodeFilterHttp<Eth: EthWrapper> {
    filter: Arc<EthFilter<Eth>>,
    provider: Arc<Eth::Provider>,
    default_compliant: bool,
}

impl<Eth: EthWrapper> HlNodeFilterHttp<Eth> {
    pub fn new(
        filter: Arc<EthFilter<Eth>>,
        provider: Arc<Eth::Provider>,
        default_compliant: bool,
    ) -> Self {
        Self { filter, provider, default_compliant }
    }
}

/// Per-request `?hl=`-aware overrides of the log-returning `eth_` filter methods. Other filter
/// methods are left to the stock reth handler (`EthFilter` clones share state via `Arc`).
#[rpc(server, namespace = "eth")]
pub trait EthLogFilterApi<T: RpcObject> {
    #[method(name = "getLogs", with_extensions)]
    async fn logs(&self, filter: Filter) -> RpcResult<Vec<Log>>;

    #[method(name = "getFilterLogs", with_extensions)]
    async fn filter_logs(&self, id: FilterId) -> RpcResult<Vec<Log>>;

    #[method(name = "getFilterChanges", with_extensions)]
    async fn filter_changes(&self, id: FilterId) -> RpcResult<FilterChanges<T>>;
}

fn adjust_filter_changes<Eth: EthWrapper>(
    changes: FilterChanges<RpcTransaction<Eth::NetworkTypes>>,
    provider: &Eth::Provider,
) -> FilterChanges<RpcTransaction<Eth::NetworkTypes>> {
    match changes {
        FilterChanges::Logs(logs) => FilterChanges::Logs(
            logs.into_iter().filter_map(|log| adjust_log::<Eth>(log, provider)).collect(),
        ),
        other => other,
    }
}

#[async_trait]
impl<Eth: EthWrapper> EthLogFilterApiServer<RpcTransaction<Eth::NetworkTypes>>
    for HlNodeFilterHttp<Eth>
{
    async fn logs(&self, ext: &Extensions, filter: Filter) -> RpcResult<Vec<Log>> {
        trace!(target: "rpc::eth", "Serving eth_getLogs");
        let logs = EthFilterApiServer::logs(&*self.filter, filter).await?;
        if is_hl_compliant(ext, self.default_compliant) {
            Ok(logs.into_iter().filter_map(|log| adjust_log::<Eth>(log, &self.provider)).collect())
        } else {
            Ok(logs)
        }
    }

    async fn filter_logs(&self, ext: &Extensions, id: FilterId) -> RpcResult<Vec<Log>> {
        trace!(target: "rpc::eth", "Serving eth_getFilterLogs");
        let logs = self.filter.filter_logs(id).await.map_err(ErrorObject::from)?;
        if is_hl_compliant(ext, self.default_compliant) {
            Ok(logs.into_iter().filter_map(|log| adjust_log::<Eth>(log, &self.provider)).collect())
        } else {
            Ok(logs)
        }
    }

    async fn filter_changes(
        &self,
        ext: &Extensions,
        id: FilterId,
    ) -> RpcResult<FilterChanges<RpcTransaction<Eth::NetworkTypes>>> {
        trace!(target: "rpc::eth", "Serving eth_getFilterChanges");
        let changes = self.filter.filter_changes(id).await.map_err(ErrorObject::from)?;
        if is_hl_compliant(ext, self.default_compliant) {
            Ok(adjust_filter_changes::<Eth>(changes, &self.provider))
        } else {
            Ok(changes)
        }
    }
}

pub struct HlNodeFilterWs<Eth: EthWrapper> {
    pubsub: Arc<EthPubSub<Eth>>,
    provider: Arc<Eth::Provider>,
    subscription_task_spawner: Box<dyn TaskSpawner + 'static>,
    default_compliant: bool,
}

impl<Eth: EthWrapper> HlNodeFilterWs<Eth> {
    pub fn new(
        pubsub: Arc<EthPubSub<Eth>>,
        provider: Arc<Eth::Provider>,
        subscription_task_spawner: Box<dyn TaskSpawner + 'static>,
        default_compliant: bool,
    ) -> Self {
        Self { pubsub, provider, subscription_task_spawner, default_compliant }
    }
}

/// Per-request `?hl=`-aware `eth_subscribe`; only `logs` subscriptions are filtered.
#[rpc(server, namespace = "eth")]
pub trait EthHlPubSubApi {
    #[subscription(
        name = "subscribe" => "subscription",
        unsubscribe = "unsubscribe",
        item = alloy_rpc_types::pubsub::SubscriptionResult,
        with_extensions
    )]
    async fn subscribe(
        &self,
        kind: SubscriptionKind,
        params: Option<Params>,
    ) -> jsonrpsee::core::SubscriptionResult;
}

#[async_trait]
impl<Eth: EthWrapper> EthHlPubSubApiServer for HlNodeFilterWs<Eth>
where
    jsonrpsee_types::error::ErrorObject<'static>: From<<Eth as EthApiTypes>::Error>,
{
    async fn subscribe(
        &self,
        pending: PendingSubscriptionSink,
        ext: &Extensions,
        kind: SubscriptionKind,
        params: Option<Params>,
    ) -> jsonrpsee::core::SubscriptionResult {
        // resolve before spawning; `ext` is borrowed
        let compliant = is_hl_compliant(ext, self.default_compliant);
        let sink = pending.accept().await?;
        let (pubsub, provider) = (self.pubsub.clone(), self.provider.clone());
        self.subscription_task_spawner.spawn(Box::pin(async move {
            if kind == SubscriptionKind::Logs {
                let filter = match params {
                    Some(Params::Logs(f)) => *f,
                    Some(Params::Bool(_)) => return,
                    _ => Default::default(),
                };
                if compliant {
                    let _ = pipe_from_stream(
                        sink,
                        pubsub
                            .log_stream(filter)
                            .filter_map(|log| adjust_log::<Eth>(log, &provider)),
                    )
                    .await;
                } else {
                    let _ = pipe_from_stream(sink, pubsub.log_stream(filter)).await;
                }
            } else if kind == SubscriptionKind::NewHeads {
                let _ = pipe_from_stream(sink, new_headers_stream::<Eth>(&provider)).await;
            } else {
                let _ = pubsub.handle_accepted(sink, kind, params).await;
            }
        }));
        Ok(())
    }
}

fn adjust_log<Eth: EthWrapper>(mut log: Log, provider: &Eth::Provider) -> Option<Log> {
    let (tx_idx, log_idx) = (log.transaction_index?, log.log_index?);
    let receipts = provider.receipts_by_block(log.block_number?.into()).unwrap()?;
    let (mut sys_tx_count, mut sys_log_count) = (0u64, 0u64);
    for receipt in receipts {
        if receipt.cumulative_gas_used() == 0 {
            sys_tx_count += 1;
            sys_log_count += receipt.logs().len() as u64;
        }
    }
    if sys_tx_count > tx_idx {
        return None;
    }
    log.transaction_index = Some(tx_idx - sys_tx_count);
    log.log_index = Some(log_idx - sys_log_count);
    Some(log)
}

pub struct HlNodeBlockFilterHttp<Eth: EthWrapper> {
    eth_api: Arc<Eth>,
    default_compliant: bool,
    _marker: PhantomData<Eth>,
}

impl<Eth: EthWrapper> HlNodeBlockFilterHttp<Eth> {
    pub fn new(eth_api: Arc<Eth>, default_compliant: bool) -> Self {
        Self { eth_api, default_compliant, _marker: PhantomData }
    }
}

#[rpc(server, namespace = "eth")]
pub trait EthBlockApi<B: RpcObject, R: RpcObject> {
    /// Returns information about a block by hash.
    #[method(name = "getBlockByHash", with_extensions)]
    async fn block_by_hash(&self, hash: B256, full: bool) -> RpcResult<Option<B>>;

    /// Returns information about a block by number.
    #[method(name = "getBlockByNumber", with_extensions)]
    async fn block_by_number(&self, number: BlockNumberOrTag, full: bool) -> RpcResult<Option<B>>;

    /// Returns all transaction receipts for a given block.
    #[method(name = "getBlockReceipts", with_extensions)]
    async fn block_receipts(&self, block_id: BlockId) -> RpcResult<Option<Vec<R>>>;

    /// Returns all transaction receipts for a given block, including system transactions.
    #[method(name = "getBlockReceiptsWithSystemTx", with_extensions)]
    async fn block_receipts_with_system_tx(
        &self,
        block_id: BlockId,
    ) -> RpcResult<Option<BlockReceiptsWithSystemTx<R>>>;

    #[method(name = "getBlockTransactionCountByHash", with_extensions)]
    async fn block_transaction_count_by_hash(&self, hash: B256) -> RpcResult<Option<U256>>;

    #[method(name = "getBlockTransactionCountByNumber", with_extensions)]
    async fn block_transaction_count_by_number(
        &self,
        number: BlockNumberOrTag,
    ) -> RpcResult<Option<U256>>;

    #[method(name = "getTransactionReceipt", with_extensions)]
    async fn transaction_receipt(&self, hash: B256) -> RpcResult<Option<R>>;
}

macro_rules! engine_span {
    () => {
        tracing::trace_span!(target: "rpc", "engine")
    };
}

fn adjust_block<Eth: EthWrapper>(
    recovered_block: &RpcBlock<Eth::NetworkTypes>,
    eth_api: &Eth,
) -> RpcBlock<Eth::NetworkTypes> {
    let system_tx_count = system_tx_count_for_block(eth_api, recovered_block.number().into());
    let mut new_block = recovered_block.clone();

    new_block.transactions = match new_block.transactions {
        BlockTransactions::Full(mut transactions) => {
            transactions.drain(..system_tx_count);
            transactions.iter_mut().for_each(|tx| {
                if let Some(idx) = &mut tx.transaction_index {
                    *idx -= system_tx_count as u64;
                }
            });
            BlockTransactions::Full(transactions)
        }
        BlockTransactions::Hashes(mut hashes) => {
            hashes.drain(..system_tx_count);
            BlockTransactions::Hashes(hashes)
        }
        BlockTransactions::Uncle => BlockTransactions::Uncle,
    };
    new_block
}

async fn adjust_block_receipts<Eth: EthWrapper>(
    block_id: BlockId,
    eth_api: &Eth,
) -> Result<Option<(usize, Vec<RpcReceipt<Eth::NetworkTypes>>)>, Eth::Error> {
    // Modified from EthBlocks::block_receipt. See `NOTE` comment below.
    let system_tx_count = system_tx_count_for_block(eth_api, block_id);
    if let Some((block, receipts)) = EthBlocks::load_block_and_receipts(eth_api, block_id).await? {
        let block_number = block.number;
        let base_fee = block.base_fee_per_gas;
        let block_hash = block.hash();
        let excess_blob_gas = block.excess_blob_gas;
        let timestamp = block.timestamp;
        let mut gas_used = 0;
        let mut next_log_index = 0;

        let inputs = block
            .transactions_recovered()
            .zip(receipts.iter())
            .enumerate()
            .filter_map(|(idx, (tx, receipt))| {
                if receipt.cumulative_gas_used() == 0 {
                    // NOTE: modified to exclude system tx
                    return None;
                }
                let meta = TransactionMeta {
                    tx_hash: *tx.tx_hash(),
                    index: (idx - system_tx_count) as u64,
                    block_hash,
                    block_number,
                    base_fee,
                    excess_blob_gas,
                    timestamp,
                };

                let input = ConvertReceiptInput {
                    receipt: receipt.clone(),
                    tx,
                    gas_used: receipt.cumulative_gas_used() - gas_used,
                    next_log_index,
                    meta,
                };

                gas_used = receipt.cumulative_gas_used();
                next_log_index += receipt.logs().len();

                Some(input)
            })
            .collect::<Vec<_>>();

        return eth_api
            .tx_resp_builder()
            .convert_receipts(inputs)
            .map(|receipts| Some((system_tx_count, receipts)));
    }

    Ok(None)
}

async fn block_receipts_with_system_txs<Eth: EthWrapper>(
    block_id: BlockId,
    eth_api: &Eth,
) -> Result<Option<BlockReceiptsWithSystemTx<RpcReceipt<Eth::NetworkTypes>>>, Eth::Error> {
    let system_tx_count = system_tx_count_for_block(eth_api, block_id);
    if let Some((block, receipts)) = EthBlocks::load_block_and_receipts(eth_api, block_id).await? {
        let block_number = block.number;
        let base_fee = block.base_fee_per_gas;
        let block_hash = block.hash();
        let excess_blob_gas = block.excess_blob_gas;
        let timestamp = block.timestamp;
        let mut regular_gas_used = 0;
        let mut regular_next_log_index = 0;
        let mut system_gas_used = 0;
        let mut system_next_log_index = 0;

        let mut regular_inputs = Vec::new();
        let mut system_inputs = Vec::new();
        for (idx, (tx, receipt)) in block.transactions_recovered().zip(receipts.iter()).enumerate()
        {
            if idx < system_tx_count {
                let meta = TransactionMeta {
                    tx_hash: *tx.tx_hash(),
                    index: idx as u64,
                    block_hash,
                    block_number,
                    base_fee,
                    excess_blob_gas,
                    timestamp,
                };

                let input = ConvertReceiptInput {
                    receipt: receipt.clone(),
                    tx,
                    gas_used: receipt.cumulative_gas_used() - system_gas_used,
                    next_log_index: system_next_log_index,
                    meta,
                };

                system_gas_used = receipt.cumulative_gas_used();
                system_next_log_index += receipt.logs().len();
                system_inputs.push(input);
                continue;
            }

            let meta = TransactionMeta {
                tx_hash: *tx.tx_hash(),
                index: (idx - system_tx_count) as u64,
                block_hash,
                block_number,
                base_fee,
                excess_blob_gas,
                timestamp,
            };

            let input = ConvertReceiptInput {
                receipt: receipt.clone(),
                tx,
                gas_used: receipt.cumulative_gas_used() - regular_gas_used,
                next_log_index: regular_next_log_index,
                meta,
            };

            regular_gas_used = receipt.cumulative_gas_used();
            regular_next_log_index += receipt.logs().len();
            regular_inputs.push(input);
        }

        let receipts = eth_api.tx_resp_builder().convert_receipts(regular_inputs)?;
        let system_tx_receipts = eth_api.tx_resp_builder().convert_receipts(system_inputs)?;
        return Ok(Some(BlockReceiptsWithSystemTx { receipts, system_tx_receipts }));
    }

    Ok(None)
}

async fn adjust_transaction_receipt<Eth: EthWrapper>(
    tx_hash: B256,
    eth_api: &Eth,
) -> Result<Option<RpcReceipt<Eth::NetworkTypes>>, Eth::Error> {
    match eth_api.load_transaction_and_receipt(tx_hash).await? {
        Some((_, meta, _)) => {
            // LoadReceipt::block_transaction_receipt loads the block again, so loading blocks again
            // doesn't hurt performance much
            let Some((system_tx_count, block_receipts)) =
                adjust_block_receipts(meta.block_hash.into(), eth_api).await?
            else {
                unreachable!();
            };
            Ok(Some(block_receipts.into_iter().nth(meta.index as usize - system_tx_count).unwrap()))
        }
        None => Ok(None),
    }
}

// This function assumes that `block_id` is already validated by the caller.
fn system_tx_count_for_block<Eth: EthWrapper>(eth_api: &Eth, block_id: BlockId) -> usize {
    let provider = eth_api.provider();
    let header = provider.header_by_id(block_id).unwrap().unwrap();

    header.extras.system_tx_count.try_into().unwrap()
}

#[async_trait]
impl<Eth: EthWrapper> EthBlockApiServer<RpcBlock<Eth::NetworkTypes>, RpcReceipt<Eth::NetworkTypes>>
    for HlNodeBlockFilterHttp<Eth>
where
    Eth: EthApiTypes + 'static,
    ErrorObject<'static>: From<Eth::Error>,
{
    /// Handler for: `eth_getBlockByHash`
    async fn block_by_hash(
        &self,
        ext: &Extensions,
        hash: B256,
        full: bool,
    ) -> RpcResult<Option<RpcBlock<Eth::NetworkTypes>>> {
        let res = self.eth_api.block_by_hash(hash, full).instrument(engine_span!()).await?;
        if is_hl_compliant(ext, self.default_compliant) {
            Ok(res.map(|block| adjust_block(&block, &*self.eth_api)))
        } else {
            Ok(res)
        }
    }

    /// Handler for: `eth_getBlockByNumber`
    async fn block_by_number(
        &self,
        ext: &Extensions,
        number: BlockNumberOrTag,
        full: bool,
    ) -> RpcResult<Option<RpcBlock<Eth::NetworkTypes>>> {
        trace!(target: "rpc::eth", ?number, ?full, "Serving eth_getBlockByNumber");
        let res = self.eth_api.block_by_number(number, full).instrument(engine_span!()).await?;
        if is_hl_compliant(ext, self.default_compliant) {
            Ok(res.map(|block| adjust_block(&block, &*self.eth_api)))
        } else {
            Ok(res)
        }
    }

    /// Handler for: `eth_getBlockTransactionCountByHash`
    async fn block_transaction_count_by_hash(
        &self,
        ext: &Extensions,
        hash: B256,
    ) -> RpcResult<Option<U256>> {
        trace!(target: "rpc::eth", ?hash, "Serving eth_getBlockTransactionCountByHash");
        let res =
            self.eth_api.block_transaction_count_by_hash(hash).instrument(engine_span!()).await?;
        if is_hl_compliant(ext, self.default_compliant) {
            Ok(res.map(|count| {
                let sys_tx_count =
                    system_tx_count_for_block(&*self.eth_api, BlockId::Hash(hash.into()));
                count - U256::from(sys_tx_count)
            }))
        } else {
            Ok(res)
        }
    }

    /// Handler for: `eth_getBlockTransactionCountByNumber`
    async fn block_transaction_count_by_number(
        &self,
        ext: &Extensions,
        number: BlockNumberOrTag,
    ) -> RpcResult<Option<U256>> {
        trace!(target: "rpc::eth", ?number, "Serving eth_getBlockTransactionCountByNumber");
        let res = self
            .eth_api
            .block_transaction_count_by_number(number)
            .instrument(engine_span!())
            .await?;
        if is_hl_compliant(ext, self.default_compliant) {
            Ok(res.map(|count| {
                count - U256::from(system_tx_count_for_block(&*self.eth_api, number.into()))
            }))
        } else {
            Ok(res)
        }
    }

    async fn transaction_receipt(
        &self,
        ext: &Extensions,
        hash: B256,
    ) -> RpcResult<Option<RpcReceipt<Eth::NetworkTypes>>> {
        trace!(target: "rpc::eth", ?hash, "Serving eth_getTransactionReceipt");
        if is_hl_compliant(ext, self.default_compliant) {
            let eth_api = &*self.eth_api;
            Ok(adjust_transaction_receipt(hash, eth_api).instrument(engine_span!()).await?)
        } else {
            Ok(EthTransactions::transaction_receipt(&*self.eth_api, hash)
                .instrument(engine_span!())
                .await?)
        }
    }

    /// Handler for: `eth_getBlockReceipts`
    async fn block_receipts(
        &self,
        ext: &Extensions,
        block_id: BlockId,
    ) -> RpcResult<Option<Vec<RpcReceipt<Eth::NetworkTypes>>>> {
        trace!(target: "rpc::eth", ?block_id, "Serving eth_getBlockReceipts");
        if self.eth_api.provider().block_by_id(block_id).map_err(EthApiError::from)?.is_none() {
            return Ok(None);
        }
        if is_hl_compliant(ext, self.default_compliant) {
            let result =
                adjust_block_receipts(block_id, &*self.eth_api).instrument(engine_span!()).await?;
            Ok(result.map(|(_, receipts)| receipts))
        } else {
            Ok(EthBlocks::block_receipts(&*self.eth_api, block_id)
                .instrument(engine_span!())
                .await?)
        }
    }

    /// Handler for: `eth_getBlockReceiptsWithSystemTx`
    async fn block_receipts_with_system_tx(
        &self,
        _ext: &Extensions,
        block_id: BlockId,
    ) -> RpcResult<Option<BlockReceiptsWithSystemTx<RpcReceipt<Eth::NetworkTypes>>>> {
        trace!(target: "rpc::eth", ?block_id, "Serving eth_getBlockReceiptsWithSystemTx");
        if self.eth_api.provider().block_by_id(block_id).map_err(EthApiError::from)?.is_none() {
            return Ok(None);
        }
        let result = block_receipts_with_system_txs(block_id, &*self.eth_api)
            .instrument(engine_span!())
            .await?;
        Ok(result)
    }
}

pub fn install<Node, EthApi>(
    ctx: &mut RpcContext<Node, EthApi>,
    default_compliant: bool,
) -> Result<(), eyre::Error>
where
    Node: FullNodeComponents,
    Node::Provider: BlockIdReader + BlockReader<Block = crate::HlBlock>,
    EthApi: EthWrapper,
    ErrorObject<'static>: From<EthApi::Error>,
{
    // Installed unconditionally so `?hl=` works in both directions; absent the extension,
    // `is_hl_compliant` falls back to `default_compliant`.
    ctx.modules.replace_configured(
        HlNodeFilterHttp::new(
            Arc::new(ctx.registry.eth_handlers().filter.clone()),
            Arc::new(ctx.registry.eth_api().provider().clone()),
            default_compliant,
        )
        .into_rpc(),
    )?;
    ctx.modules.replace_configured(
        HlNodeFilterWs::new(
            Arc::new(ctx.registry.eth_handlers().pubsub.clone()),
            Arc::new(ctx.registry.eth_api().provider().clone()),
            Box::new(ctx.node().task_executor().clone()),
            default_compliant,
        )
        .into_rpc(),
    )?;

    ctx.modules.replace_configured(
        HlNodeBlockFilterHttp::new(Arc::new(ctx.registry.eth_api().clone()), default_compliant)
            .into_rpc(),
    )?;

    ctx.modules
        .merge_configured(HlSystemTransactionExt::new(ctx.registry.eth_api().clone()).into_rpc())?;

    Ok(())
}
