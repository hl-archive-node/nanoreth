#![allow(clippy::owned_cow)]
use super::{HlBlock, HlBlockBody, TransactionSigned};
use crate::{HlHeader, node::types::ReadPrecompileCalls};
use alloy_consensus::{BlobTransactionSidecar, BlockBody};
use alloy_eips::eip4895::Withdrawals;
use alloy_primitives::{Address, B256};
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};
use std::borrow::Cow;

#[derive(RlpEncodable, RlpDecodable)]
#[rlp(trailing)]
struct BlockBodyHelper<'a> {
    transactions: Cow<'a, Vec<TransactionSigned>>,
    ommers: Cow<'a, Vec<HlHeader>>,
    withdrawals: Option<Cow<'a, Withdrawals>>,
    sidecars: Option<Cow<'a, Vec<BlobTransactionSidecar>>>,
    read_precompile_calls: Option<Cow<'a, ReadPrecompileCalls>>,
    highest_precompile_address: Option<Cow<'a, Address>>,
    system_tx_receipts_hash: Option<B256>,
}

#[derive(RlpEncodable, RlpDecodable)]
#[rlp(trailing)]
pub(crate) struct BlockHelper<'a> {
    pub(crate) header: Cow<'a, HlHeader>,
    pub(crate) transactions: Cow<'a, Vec<TransactionSigned>>,
    pub(crate) ommers: Cow<'a, Vec<HlHeader>>,
    pub(crate) withdrawals: Option<Cow<'a, Withdrawals>>,
    pub(crate) sidecars: Option<Cow<'a, Vec<BlobTransactionSidecar>>>,
    pub(crate) read_precompile_calls: Option<Cow<'a, ReadPrecompileCalls>>,
    pub(crate) highest_precompile_address: Option<Cow<'a, Address>>,
    pub(crate) system_tx_receipts_hash: Option<B256>,
}

impl<'a> From<&'a HlBlockBody> for BlockBodyHelper<'a> {
    fn from(value: &'a HlBlockBody) -> Self {
        let HlBlockBody {
            inner: BlockBody { transactions, ommers, withdrawals },
            sidecars,
            read_precompile_calls,
            highest_precompile_address,
            system_tx_receipts_hash,
        } = value;
        Self {
            transactions: Cow::Borrowed(transactions),
            ommers: Cow::Borrowed(ommers),
            withdrawals: withdrawals.as_ref().map(Cow::Borrowed),
            sidecars: sidecars.as_ref().map(Cow::Borrowed),
            read_precompile_calls: read_precompile_calls.as_ref().map(Cow::Borrowed),
            highest_precompile_address: highest_precompile_address.as_ref().map(Cow::Borrowed),
            system_tx_receipts_hash: *system_tx_receipts_hash,
        }
    }
}

impl<'a> From<&'a HlBlock> for BlockHelper<'a> {
    fn from(value: &'a HlBlock) -> Self {
        let HlBlock {
            header,
            body:
                HlBlockBody {
                    inner: BlockBody { transactions, ommers, withdrawals },
                    sidecars,
                    read_precompile_calls,
                    highest_precompile_address,
                    system_tx_receipts_hash,
                },
        } = value;
        Self {
            header: Cow::Borrowed(header),
            transactions: Cow::Borrowed(transactions),
            ommers: Cow::Borrowed(ommers),
            withdrawals: withdrawals.as_ref().map(Cow::Borrowed),
            sidecars: sidecars.as_ref().map(Cow::Borrowed),
            read_precompile_calls: read_precompile_calls.as_ref().map(Cow::Borrowed),
            highest_precompile_address: highest_precompile_address.as_ref().map(Cow::Borrowed),
            system_tx_receipts_hash: *system_tx_receipts_hash,
        }
    }
}

impl Encodable for HlBlockBody {
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        BlockBodyHelper::from(self).encode(out);
    }
    fn length(&self) -> usize {
        BlockBodyHelper::from(self).length()
    }
}

impl Decodable for HlBlockBody {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let BlockBodyHelper {
            transactions,
            ommers,
            withdrawals,
            sidecars,
            read_precompile_calls,
            highest_precompile_address,
            system_tx_receipts_hash,
        } = BlockBodyHelper::decode(buf)?;
        Ok(Self {
            inner: BlockBody {
                transactions: transactions.into_owned(),
                ommers: ommers.into_owned(),
                withdrawals: withdrawals.map(|w| w.into_owned()),
            },
            sidecars: sidecars.map(|s| s.into_owned()),
            read_precompile_calls: read_precompile_calls.map(|s| s.into_owned()),
            highest_precompile_address: highest_precompile_address.map(|s| s.into_owned()),
            system_tx_receipts_hash,
        })
    }
}

impl Encodable for HlBlock {
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        BlockHelper::from(self).encode(out);
    }
    fn length(&self) -> usize {
        BlockHelper::from(self).length()
    }
}

impl Decodable for HlBlock {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let BlockHelper {
            header,
            transactions,
            ommers,
            withdrawals,
            sidecars,
            read_precompile_calls,
            highest_precompile_address,
            system_tx_receipts_hash,
        } = BlockHelper::decode(buf)?;
        Ok(Self {
            header: header.into_owned(),
            body: HlBlockBody {
                inner: BlockBody {
                    transactions: transactions.into_owned(),
                    ommers: ommers.into_owned(),
                    withdrawals: withdrawals.map(|w| w.into_owned()),
                },
                sidecars: sidecars.map(|s| s.into_owned()),
                read_precompile_calls: read_precompile_calls.map(|s| s.into_owned()),
                highest_precompile_address: highest_precompile_address.map(|s| s.into_owned()),
                system_tx_receipts_hash,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_tx_receipts_hash_roundtrips() {
        let body = HlBlockBody {
            inner: BlockBody { transactions: vec![], ommers: vec![], withdrawals: None },
            sidecars: None,
            read_precompile_calls: Some(Default::default()),
            highest_precompile_address: None,
            system_tx_receipts_hash: Some(B256::repeat_byte(1)),
        };
        let encoded = alloy_rlp::encode(&body);
        assert_eq!(HlBlockBody::decode(&mut encoded.as_slice()).unwrap(), body);

        let block = HlBlock { header: Default::default(), body };
        let encoded = alloy_rlp::encode(&block);
        assert_eq!(encoded.len(), block.length());
        assert_eq!(HlBlock::decode(&mut encoded.as_slice()).unwrap(), block);
    }
}
