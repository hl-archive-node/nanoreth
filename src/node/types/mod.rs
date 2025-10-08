//! Extends from https://github.com/hyperliquid-dex/hyper-evm-sync
//!
//! Changes:
//! - ReadPrecompileCalls supports RLP encoding / decoding
use alloy_primitives::{Address, Bytes, Log, B256};
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};
use bytes::BufMut;
use serde::{Deserialize, Serialize};

use crate::HlBlock;

pub type ReadPrecompileCall = (Address, Vec<(ReadPrecompileInput, ReadPrecompileResult)>);

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default)]
pub struct ReadPrecompileCalls(pub Vec<ReadPrecompileCall>);

pub(crate) mod reth_compat;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HlExtras {
    pub read_precompile_calls: Option<ReadPrecompileCalls>,
    pub highest_precompile_address: Option<Address>,
}

impl Encodable for ReadPrecompileCalls {
    fn encode(&self, out: &mut dyn BufMut) {
        let buf: Bytes = rmp_serde::to_vec(&self.0).unwrap().into();
        buf.encode(out);
    }
}

impl Decodable for ReadPrecompileCalls {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let bytes = Bytes::decode(buf)?;
        let calls = rmp_serde::decode::from_slice(&bytes)
            .map_err(|_| alloy_rlp::Error::Custom("Failed to decode ReadPrecompileCalls"))?;
        Ok(Self(calls))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct BlockAndReceipts {
    pub block: EvmBlock,
    pub receipts: Vec<LegacyReceipt>,
    #[serde(default)]
    pub system_txs: Vec<SystemTx>,
    #[serde(default)]
    pub read_precompile_calls: ReadPrecompileCalls,
    pub highest_precompile_address: Option<Address>,
}

impl BlockAndReceipts {
    pub fn to_reth_block(self, chain_id: u64) -> HlBlock {
        let EvmBlock::Reth115(block) = self.block;
        block.to_reth_block(
            self.read_precompile_calls.clone(),
            self.highest_precompile_address,
            self.system_txs.clone(),
            chain_id,
        )
    }

    pub fn hash(&self) -> B256 {
        let EvmBlock::Reth115(block) = &self.block;
        block.header.hash
    }

    pub fn number(&self) -> u64 {
        let EvmBlock::Reth115(block) = &self.block;
        block.header.header.number
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum EvmBlock {
    Reth115(reth_compat::SealedBlock),
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct LegacyReceipt {
    tx_type: LegacyTxType,
    success: bool,
    cumulative_gas_used: u64,
    logs: Vec<Log>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
enum LegacyTxType {
    Legacy = 0,
    Eip2930 = 1,
    Eip1559 = 2,
    Eip4844 = 3,
    Eip7702 = 4,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct SystemTx {
    pub tx: reth_compat::Transaction,
    pub receipt: Option<LegacyReceipt>,
}

#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    Hash,
    RlpEncodable,
    RlpDecodable,
)]
pub struct ReadPrecompileInput {
    pub input: Bytes,
    pub gas_limit: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum ReadPrecompileResult {
    Ok { gas_used: u64, bytes: Bytes },
    OutOfGas,
    Error,
    UnexpectedError,
}
