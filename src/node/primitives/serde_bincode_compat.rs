#![allow(clippy::owned_cow)]
use alloy_consensus::BlobTransactionSidecar;
use alloy_primitives::{Address, B256};
use reth_primitives_traits::serde_bincode_compat::{BincodeReprFor, SerdeBincodeCompat};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

use super::{HlBlock, HlBlockBody};
use crate::{
    HlHeader,
    node::{primitives::BlockBody, types::ReadPrecompileCalls},
};

#[derive(Debug, Serialize, Deserialize)]
pub struct HlBlockBodyBincode<'a> {
    inner: BincodeReprFor<'a, BlockBody>,
    sidecars: Option<Cow<'a, Vec<BlobTransactionSidecar>>>,
    read_precompile_calls: Option<Cow<'a, ReadPrecompileCalls>>,
    highest_precompile_address: Option<Cow<'a, Address>>,
    system_tx_receipts_hash: Option<B256>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HlBlockBincode<'a> {
    header: BincodeReprFor<'a, HlHeader>,
    body: BincodeReprFor<'a, HlBlockBody>,
}

impl SerdeBincodeCompat for HlBlockBody {
    type BincodeRepr<'a> = HlBlockBodyBincode<'a>;

    fn as_repr(&self) -> Self::BincodeRepr<'_> {
        HlBlockBodyBincode {
            inner: self.inner.as_repr(),
            sidecars: self.sidecars.as_ref().map(Cow::Borrowed),
            read_precompile_calls: self.read_precompile_calls.as_ref().map(Cow::Borrowed),
            highest_precompile_address: self.highest_precompile_address.as_ref().map(Cow::Borrowed),
            system_tx_receipts_hash: self.system_tx_receipts_hash,
        }
    }

    fn from_repr(repr: Self::BincodeRepr<'_>) -> Self {
        let HlBlockBodyBincode {
            inner,
            sidecars,
            read_precompile_calls,
            highest_precompile_address,
            system_tx_receipts_hash,
        } = repr;
        Self {
            inner: BlockBody::from_repr(inner),
            sidecars: sidecars.map(|s| s.into_owned()),
            read_precompile_calls: read_precompile_calls.map(|s| s.into_owned()),
            highest_precompile_address: highest_precompile_address.map(|s| s.into_owned()),
            system_tx_receipts_hash,
        }
    }
}

impl SerdeBincodeCompat for HlBlock {
    type BincodeRepr<'a> = HlBlockBincode<'a>;

    fn as_repr(&self) -> Self::BincodeRepr<'_> {
        HlBlockBincode { header: self.header.as_repr(), body: self.body.as_repr() }
    }

    fn from_repr(repr: Self::BincodeRepr<'_>) -> Self {
        let HlBlockBincode { header, body } = repr;
        Self { header: HlHeader::from_repr(header), body: HlBlockBody::from_repr(body) }
    }
}
