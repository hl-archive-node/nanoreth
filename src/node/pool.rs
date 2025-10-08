//! A placeholder for the transaction pool.
//!
//! We'll not use transaction pool in HL, since this node does not write to HL consensus;
//! just a minimum implementation to make the node compile would suffice.
//!
//! Ethereum transaction pool only supports TransactionSigned (EthereumTxEnvelope<TxEip4844>),
//! hence this placeholder for the transaction pool.

use crate::node::{primitives::TransactionSigned, HlNode};
use alloy_consensus::{
    error::ValueError, EthereumTxEnvelope, Transaction as TransactionTrait, TxEip4844,
};
use alloy_eips::{eip7702::SignedAuthorization, Typed2718};
use alloy_primitives::{Address, Bytes, ChainId, TxHash, TxKind, B256, U256};
use alloy_rpc_types::AccessList;
use reth::{
    api::FullNodeTypes, builder::components::PoolBuilder, transaction_pool::PoolTransaction,
};
use reth_ethereum_primitives::PooledTransactionVariant;
use reth_primitives::Recovered;
use reth_primitives_traits::InMemorySize;
use reth_transaction_pool::{noop::NoopTransactionPool, EthPoolTransaction};
use std::sync::Arc;

pub struct HlPoolBuilder;
impl<Node> PoolBuilder<Node> for HlPoolBuilder
where
    Node: FullNodeTypes<Types = HlNode>,
{
    type Pool = NoopTransactionPool<HlPooledTransaction>;

    async fn build_pool(
        self,
        _ctx: &reth::builder::BuilderContext<Node>,
    ) -> eyre::Result<Self::Pool> {
        Ok(NoopTransactionPool::new())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HlPooledTransaction;

impl InMemorySize for HlPooledTransaction {
    fn size(&self) -> usize {
        0
    }
}

impl TransactionTrait for HlPooledTransaction {
    fn chain_id(&self) -> Option<ChainId> {
        unreachable!()
    }
    fn nonce(&self) -> u64 {
        unreachable!()
    }
    fn gas_limit(&self) -> u64 {
        unreachable!()
    }
    fn gas_price(&self) -> Option<u128> {
        unreachable!()
    }
    fn max_fee_per_gas(&self) -> u128 {
        unreachable!()
    }
    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        unreachable!()
    }
    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        unreachable!()
    }
    fn priority_fee_or_price(&self) -> u128 {
        unreachable!()
    }
    fn effective_gas_price(&self, _base_fee: Option<u64>) -> u128 {
        unreachable!()
    }
    fn is_dynamic_fee(&self) -> bool {
        unreachable!()
    }
    fn kind(&self) -> TxKind {
        unreachable!()
    }
    fn is_create(&self) -> bool {
        unreachable!()
    }
    fn value(&self) -> U256 {
        unreachable!()
    }
    fn input(&self) -> &Bytes {
        unreachable!()
    }
    fn access_list(&self) -> Option<&AccessList> {
        unreachable!()
    }
    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        unreachable!()
    }
    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        unreachable!()
    }
}

impl Typed2718 for HlPooledTransaction {
    fn ty(&self) -> u8 {
        unreachable!()
    }
}

impl PoolTransaction for HlPooledTransaction {
    type TryFromConsensusError = ValueError<EthereumTxEnvelope<TxEip4844>>;
    type Consensus = TransactionSigned;
    type Pooled = PooledTransactionVariant;

    fn into_consensus(self) -> Recovered<Self::Consensus> {
        unreachable!()
    }

    fn from_pooled(_tx: Recovered<Self::Pooled>) -> Self {
        unreachable!()
    }

    fn hash(&self) -> &TxHash {
        unreachable!()
    }

    fn sender(&self) -> Address {
        unreachable!()
    }

    fn sender_ref(&self) -> &Address {
        unreachable!()
    }

    fn cost(&self) -> &U256 {
        unreachable!()
    }

    fn encoded_length(&self) -> usize {
        0
    }
}

impl EthPoolTransaction for HlPooledTransaction {
    fn take_blob(&mut self) -> reth_transaction_pool::EthBlobTransactionSidecar {
        unreachable!()
    }

    fn try_into_pooled_eip4844(
        self,
        _sidecar: Arc<alloy_eips::eip7594::BlobTransactionSidecarVariant>,
    ) -> Option<Recovered<Self::Pooled>> {
        None
    }

    fn try_from_eip4844(
        _tx: Recovered<Self::Consensus>,
        _sidecar: alloy_eips::eip7594::BlobTransactionSidecarVariant,
    ) -> Option<Self> {
        None
    }

    fn validate_blob(
        &self,
        _blob: &alloy_eips::eip7594::BlobTransactionSidecarVariant,
        _settings: &alloy_eips::eip4844::env_settings::KzgSettings,
    ) -> Result<(), alloy_consensus::BlobTransactionValidationError> {
        Ok(())
    }
}
