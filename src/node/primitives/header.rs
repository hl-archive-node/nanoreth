use alloy_consensus::Header;
use alloy_primitives::{Address, B64, B256, BlockNumber, Bloom, Bytes, Sealable, U256};
use alloy_rlp::{RlpDecodable, RlpEncodable};
use reth_codecs::Compact;
use reth_ethereum_primitives::EthereumReceipt;
use reth_primitives_traits::{SealedHeader, logs_bloom};
use reth_primitives_traits::{BlockHeader, InMemorySize, header::HeaderMut};
use reth_rpc_convert::FromConsensusHeader;
use serde::{Deserialize, Serialize};

/// The header type of this node
///
/// This type extends the regular ethereum header with an extension.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    Hash,
    derive_more::AsRef,
    derive_more::Deref,
    Default,
    RlpEncodable,
    RlpDecodable,
    Serialize,
    Deserialize,
)]
#[serde(rename_all = "camelCase")]
pub struct HlHeader {
    /// The regular eth header
    #[as_ref]
    #[deref]
    pub inner: Header,
    /// The extended header fields that is not part of the block hash
    pub extras: HlHeaderExtras,
}

#[derive(
    Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, RlpEncodable, RlpDecodable, Hash,
)]
pub struct HlHeaderExtras {
    pub logs_bloom_with_system_txs: Bloom,
    pub system_tx_count: u64,
}

impl HlHeader {
    pub(crate) fn from_ethereum_header(
        header: Header,
        receipts: &[EthereumReceipt],
        system_tx_count: u64,
    ) -> HlHeader {
        let logs_bloom = logs_bloom(receipts.iter().flat_map(|r| &r.logs));
        HlHeader {
            inner: header,
            extras: HlHeaderExtras { logs_bloom_with_system_txs: logs_bloom, system_tx_count },
        }
    }
}

impl From<Header> for HlHeader {
    fn from(_value: Header) -> Self {
        unreachable!()
    }
}

impl AsRef<Self> for HlHeader {
    fn as_ref(&self) -> &Self {
        self
    }
}

impl Sealable for HlHeader {
    fn hash_slow(&self) -> B256 {
        self.inner.hash_slow()
    }
}

impl alloy_consensus::BlockHeader for HlHeader {
    fn parent_hash(&self) -> B256 {
        self.inner.parent_hash()
    }

    fn ommers_hash(&self) -> B256 {
        self.inner.ommers_hash()
    }

    fn beneficiary(&self) -> Address {
        self.inner.beneficiary()
    }

    fn state_root(&self) -> B256 {
        self.inner.state_root()
    }

    fn transactions_root(&self) -> B256 {
        self.inner.transactions_root()
    }

    fn receipts_root(&self) -> B256 {
        self.inner.receipts_root()
    }

    fn withdrawals_root(&self) -> Option<B256> {
        self.inner.withdrawals_root()
    }

    fn logs_bloom(&self) -> Bloom {
        self.extras.logs_bloom_with_system_txs
    }

    fn difficulty(&self) -> U256 {
        self.inner.difficulty()
    }

    fn number(&self) -> BlockNumber {
        self.inner.number()
    }

    fn gas_limit(&self) -> u64 {
        self.inner.gas_limit()
    }

    fn gas_used(&self) -> u64 {
        self.inner.gas_used()
    }

    fn timestamp(&self) -> u64 {
        self.inner.timestamp()
    }

    fn mix_hash(&self) -> Option<B256> {
        self.inner.mix_hash()
    }

    fn nonce(&self) -> Option<B64> {
        self.inner.nonce()
    }

    fn base_fee_per_gas(&self) -> Option<u64> {
        self.inner.base_fee_per_gas()
    }

    fn blob_gas_used(&self) -> Option<u64> {
        self.inner.blob_gas_used()
    }

    fn excess_blob_gas(&self) -> Option<u64> {
        self.inner.excess_blob_gas()
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.inner.parent_beacon_block_root()
    }

    fn requests_hash(&self) -> Option<B256> {
        self.inner.requests_hash()
    }

    fn extra_data(&self) -> &Bytes {
        self.inner.extra_data()
    }

    fn is_empty(&self) -> bool {
        self.extras.system_tx_count == 0 && self.inner.is_empty()
    }
    fn block_access_list_hash(&self) -> Option<B256> {
        self.inner.block_access_list_hash()
    }

    fn slot_number(&self) -> Option<u64> {
        self.inner.slot_number()
    }

}

impl InMemorySize for HlHeader {
    fn size(&self) -> usize {
        self.inner.size() + self.extras.size()
    }
}

impl InMemorySize for HlHeaderExtras {
    fn size(&self) -> usize {
        self.logs_bloom_with_system_txs.data().len() + self.system_tx_count.size()
    }
}

impl reth_codecs::Compact for HlHeader {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: alloy_rlp::bytes::BufMut + AsMut<[u8]>,
    {
        // Because Header ends with extra_data which is `Bytes`, we can't use to_compact for extras,
        // because Compact trait requires the Bytes field to be placed at the end of the struct.
        // Bytes::from_compact just reads all trailing data as the Bytes field.
        //
        // Hence we need to use other form of serialization, since extra headers are not
        // Compact-compatible. We just treat all header fields as rmp-serialized one `Bytes`
        // field.
        let result: Bytes = rmp_serde::to_vec(&self).unwrap().into();
        result.to_compact(buf)
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let (bytes, remaining) = Bytes::from_compact(buf, len);
        let header: HlHeader = rmp_serde::from_slice(&bytes).unwrap();
        (header, remaining)
    }
}

impl reth_db_api::table::Compress for HlHeader {
    type Compressed = Vec<u8>;

    fn compress_to_buf<B: alloy_primitives::bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
        let _ = Compact::to_compact(self, buf);
    }
}

impl reth_db_api::table::Decompress for HlHeader {
    fn decompress(value: &[u8]) -> Result<Self, reth_codecs::DecompressError> {
        let (obj, _) = Compact::from_compact(value, value.len());
        Ok(obj)
    }
}

impl BlockHeader for HlHeader {}


impl HeaderMut for HlHeader {
    fn set_parent_hash(&mut self, hash: B256) {
        self.inner.set_parent_hash(hash);
    }

    fn set_block_number(&mut self, number: BlockNumber) {
        self.inner.set_block_number(number);
    }

    fn set_timestamp(&mut self, timestamp: u64) {
        self.inner.set_timestamp(timestamp);
    }

    fn set_state_root(&mut self, state_root: B256) {
        self.inner.set_state_root(state_root);
    }

    fn set_difficulty(&mut self, difficulty: U256) {
        self.inner.set_difficulty(difficulty);
    }

    fn set_mix_hash(&mut self, mix_hash: B256) {
        self.inner.set_mix_hash(mix_hash);
    }

    fn set_extra_data(&mut self, extra_data: Bytes) {
        self.inner.set_extra_data(extra_data);
    }

    fn set_parent_beacon_block_root(&mut self, parent_beacon_block_root: Option<B256>) {
        self.inner.set_parent_beacon_block_root(parent_beacon_block_root);
    }
}

impl From<HlHeader> for Header {
    fn from(value: HlHeader) -> Self {
        value.inner
    }
}

pub fn to_ethereum_ommers(ommers: &[HlHeader]) -> Vec<Header> {
    ommers.iter().map(|ommer| ommer.clone().into()).collect()
}

impl FromConsensusHeader<HlHeader> for alloy_rpc_types::Header {
    fn from_consensus_header(header: SealedHeader<HlHeader>, block_size: usize) -> Self {
        FromConsensusHeader::<Header>::from_consensus_header(
            SealedHeader::<Header>::new(header.inner.clone(), header.hash()),
            block_size,
        )
    }
}
