use super::{HlBlockBody, HlHeader, rlp};
use alloy_rlp::Encodable;
use reth_primitives_traits::{Block, InMemorySize};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// Block for HL
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HlBlock {
    pub header: HlHeader,
    pub body: HlBlockBody,
}

impl InMemorySize for HlBlock {
    fn size(&self) -> usize {
        self.header.size() + self.body.size()
    }
}

impl Block for HlBlock {
    type Header = HlHeader;
    type Body = HlBlockBody;

    fn new(header: Self::Header, body: Self::Body) -> Self {
        Self { header, body }
    }
    fn header(&self) -> &Self::Header {
        &self.header
    }
    fn body(&self) -> &Self::Body {
        &self.body
    }
    fn split(self) -> (Self::Header, Self::Body) {
        (self.header, self.body)
    }

    fn rlp_length(header: &Self::Header, body: &Self::Body) -> usize {
        rlp::BlockHelper {
            header: Cow::Borrowed(header),
            transactions: Cow::Borrowed(&body.inner.transactions),
            ommers: Cow::Borrowed(&body.inner.ommers),
            withdrawals: body.inner.withdrawals.as_ref().map(Cow::Borrowed),
            sidecars: body.sidecars.as_ref().map(Cow::Borrowed),
            read_precompile_calls: body.read_precompile_calls.as_ref().map(Cow::Borrowed),
            highest_precompile_address: body.highest_precompile_address.as_ref().map(Cow::Borrowed),
            system_tx_receipts_hash: body.system_tx_receipts_hash,
        }
        .length()
    }
}
