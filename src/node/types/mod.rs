//! Extends from https://github.com/hyperliquid-dex/hyper-evm-sync
//!
//! Changes:
//! - ReadPrecompileCalls supports RLP encoding / decoding
use alloy_consensus::TxType;
use alloy_primitives::{Address, B256, Bytes, Log};
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};
use bytes::BufMut;
use reth_ethereum_primitives::EthereumReceipt;
use reth_primitives_traits::InMemorySize;
use serde::{Deserialize, Serialize};

use crate::HlBlock;

pub type ReadPrecompileCall = (Address, Vec<(ReadPrecompileInput, ReadPrecompileResult)>);

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default, Hash)]
pub struct ReadPrecompileCalls(pub Vec<ReadPrecompileCall>);

pub(crate) mod reth_compat;

// Re-export spot metadata functions
pub use reth_compat::{initialize_spot_metadata_cache, set_spot_metadata_db};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HlExtras {
    pub read_precompile_calls: Option<ReadPrecompileCalls>,
    pub highest_precompile_address: Option<Address>,
}

impl InMemorySize for HlExtras {
    fn size(&self) -> usize {
        self.read_precompile_calls.as_ref().map_or(0, |s| s.0.len()) +
            self.highest_precompile_address.as_ref().map_or(0, |_| 20)
    }
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
            self.receipts.clone(),
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

impl LegacyReceipt {
    /// Create a new LegacyReceipt
    pub fn new(tx_type: u8, success: bool, cumulative_gas_used: u64, logs: Vec<Log>) -> Self {
        let tx_type = match tx_type {
            0 => LegacyTxType::Legacy,
            1 => LegacyTxType::Eip2930,
            2 => LegacyTxType::Eip1559,
            3 => LegacyTxType::Eip4844,
            4 => LegacyTxType::Eip7702,
            _ => LegacyTxType::Legacy,
        };
        Self { tx_type, success, cumulative_gas_used, logs }
    }
}

impl From<LegacyReceipt> for EthereumReceipt {
    fn from(r: LegacyReceipt) -> Self {
        EthereumReceipt {
            tx_type: match r.tx_type {
                LegacyTxType::Legacy => TxType::Legacy,
                LegacyTxType::Eip2930 => TxType::Eip2930,
                LegacyTxType::Eip1559 => TxType::Eip1559,
                LegacyTxType::Eip4844 => TxType::Eip4844,
                LegacyTxType::Eip7702 => TxType::Eip7702,
            },
            success: r.success,
            cumulative_gas_used: r.cumulative_gas_used,
            logs: r.logs,
        }
    }
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

impl SystemTx {
    pub fn gas_limit(&self) -> u64 {
        use reth_compat::Transaction;
        match &self.tx {
            Transaction::Legacy(tx) => tx.gas_limit,
            Transaction::Eip2930(tx) => tx.gas_limit,
            Transaction::Eip1559(tx) => tx.gas_limit,
            Transaction::Eip4844(tx) => tx.gas_limit,
            Transaction::Eip7702(tx) => tx.gas_limit,
        }
    }
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

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub enum ReadPrecompileResult {
    Ok { gas_used: u64, bytes: Bytes },
    OutOfGas,
    Error,
    UnexpectedError,
}
