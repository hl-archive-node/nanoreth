//! Extends from https://github.com/hyperliquid-dex/hyper-evm-sync
//!
//! Changes:
//! - ReadPrecompileCalls supports RLP encoding / decoding
use alloy_consensus::TxType;
use alloy_primitives::{Address, B256, Bytes, Log};
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};
use bytes::BufMut;
use reth_ethereum_primitives::EthereumReceipt;
use reth_primitives_traits::{InMemorySize, SignerRecoverable};
use serde::{Deserialize, Serialize};

use crate::HlBlock;

pub type ReadPrecompileCall = (Address, Vec<(ReadPrecompileInput, ReadPrecompileResult)>);

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default, Hash)]
pub struct ReadPrecompileCalls(pub Vec<ReadPrecompileCall>);

mod patch;
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

    /// Construct a `BlockAndReceipts` from database types (reverse of `to_reth_block`).
    ///
    /// Splits system transactions and receipts from regular ones using
    /// the `system_tx_count` stored in the header extras.
    pub fn from_db(block: HlBlock, receipts: Vec<EthereumReceipt>) -> eyre::Result<Self> {
        let system_tx_count = block.header.extras.system_tx_count as usize;
        let hash = alloy_primitives::Sealable::hash_slow(&block.header);
        let mut all_txs = block.body.inner.transactions;
        eyre::ensure!(
            system_tx_count <= all_txs.len(),
            "Block {hash} declares {system_tx_count} system transactions but contains {} transactions",
            all_txs.len()
        );
        eyre::ensure!(
            receipts.len() == all_txs.len(),
            "Block {hash} contains {} transactions but {} receipts",
            all_txs.len(),
            receipts.len()
        );

        let regular_tx_list = all_txs.split_off(system_tx_count);
        let system_tx_list = all_txs;
        let mut system_receipts = receipts;
        let regular_receipts = system_receipts.split_off(system_tx_count);

        // Convert system transactions
        let system_txs: Vec<SystemTx> = system_tx_list
            .into_iter()
            .zip(system_receipts)
            .map(|(tx, receipt)| {
                let from = tx.recover_signer().ok();
                SystemTx {
                    tx: reth_compat::TransactionSigned::extract_transaction(tx),
                    receipt: Some(receipt.into()),
                    from,
                }
            })
            .collect();

        // Convert regular transactions to reth_compat format
        let compat_txs: Vec<reth_compat::TransactionSigned> =
            regular_tx_list.into_iter().map(reth_compat::TransactionSigned::from_node_tx).collect();

        // Convert regular receipts
        let legacy_receipts: Vec<LegacyReceipt> =
            regular_receipts.into_iter().map(Into::into).collect();

        let sealed_block = reth_compat::SealedBlock {
            header: reth_compat::SealedHeader { hash, header: block.header.inner },
            body: alloy_consensus::BlockBody {
                transactions: compat_txs,
                ommers: vec![],
                withdrawals: block.body.inner.withdrawals,
            },
        };

        Ok(BlockAndReceipts {
            block: EvmBlock::Reth115(sealed_block),
            receipts: legacy_receipts,
            system_txs,
            read_precompile_calls: block.body.read_precompile_calls.unwrap_or_default(),
            highest_precompile_address: block.body.highest_precompile_address,
        })
    }

    pub fn hash(&self) -> B256 {
        let EvmBlock::Reth115(block) = &self.block;
        block.header.hash
    }

    pub fn number(&self) -> u64 {
        let EvmBlock::Reth115(block) = &self.block;
        block.header.header.number
    }

    pub fn parent_hash(&self) -> B256 {
        let EvmBlock::Reth115(block) = &self.block;
        block.header.header.parent_hash
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

impl From<EthereumReceipt> for LegacyReceipt {
    fn from(r: EthereumReceipt) -> Self {
        LegacyReceipt {
            tx_type: match r.tx_type {
                TxType::Legacy => LegacyTxType::Legacy,
                TxType::Eip2930 => LegacyTxType::Eip2930,
                TxType::Eip1559 => LegacyTxType::Eip1559,
                TxType::Eip4844 => LegacyTxType::Eip4844,
                TxType::Eip7702 => LegacyTxType::Eip7702,
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
    #[serde(default)]
    pub from: Option<Address>,
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
