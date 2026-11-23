use alloy_consensus::BlobTransactionSidecar;
use alloy_primitives::{Address, B256};
use reth_primitives_traits::{BlockBody as BlockBodyTrait, InMemorySize};
use serde::{Deserialize, Serialize};

use crate::{
    HlHeader,
    node::{
        primitives::TransactionSigned,
        types::{ReadPrecompileCall, ReadPrecompileCalls},
    },
};

/// Block body for HL. It is equivalent to Ethereum [`BlockBody`] but additionally stores sidecars
/// for blob transactions.
#[derive(
    Debug,
    Clone,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    derive_more::Deref,
    derive_more::DerefMut,
)]
pub struct HlBlockBody {
    #[serde(flatten)]
    #[deref]
    #[deref_mut]
    pub inner: BlockBody,
    pub sidecars: Option<Vec<BlobTransactionSidecar>>,
    pub read_precompile_calls: Option<ReadPrecompileCalls>,
    pub highest_precompile_address: Option<Address>,
    /// Commitment to the system tx receipts of the block source, see
    /// [`crate::node::types::system_tx_receipts_hash`]. System tx receipts are not committed to by
    /// the header, so execution checks its own against this.
    pub system_tx_receipts_hash: Option<B256>,
}

pub type BlockBody = alloy_consensus::BlockBody<TransactionSigned, HlHeader>;

impl InMemorySize for HlBlockBody {
    fn size(&self) -> usize {
        self.inner.size() +
            self.sidecars
                .as_ref()
                .map_or(0, |s| s.capacity() * core::mem::size_of::<BlobTransactionSidecar>()) +
            self.read_precompile_calls
                .as_ref()
                .map_or(0, |s| s.0.capacity() * core::mem::size_of::<ReadPrecompileCall>())
    }
}

impl BlockBodyTrait for HlBlockBody {
    type Transaction = TransactionSigned;
    type OmmerHeader = super::HlHeader;

    fn transactions(&self) -> &[Self::Transaction] {
        BlockBodyTrait::transactions(&self.inner)
    }
    fn into_ethereum_body(self) -> BlockBody {
        self.inner
    }
    fn into_transactions(self) -> Vec<Self::Transaction> {
        self.inner.into_transactions()
    }
    fn withdrawals(&self) -> Option<&alloy_rpc_types::Withdrawals> {
        self.inner.withdrawals()
    }
    fn ommers(&self) -> Option<&[Self::OmmerHeader]> {
        self.inner.ommers()
    }

    fn calculate_tx_root(&self) -> alloy_primitives::B256 {
        alloy_consensus::proofs::calculate_transaction_root(
            &self
                .transactions()
                .iter()
                .filter(|tx| !tx.is_system_transaction())
                .collect::<Vec<_>>(),
        )
    }
}
