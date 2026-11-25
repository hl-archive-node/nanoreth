//! Extends from https://github.com/hyperliquid-dex/hyper-evm-sync
//!
//! Changes:
//! - ReadPrecompileCalls supports RLP encoding / decoding
use alloy_consensus::TxType;
use alloy_primitives::{Address, B256, Bytes, Log, keccak256};
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable, encode};
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
    /// See [`HlBlockBody::system_tx_receipts_hash`](crate::HlBlockBody). Last, and skipped when
    /// absent, so rows written without it (and the `eth_blockPrecompileData` output) are
    /// unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_tx_receipts_hash: Option<B256>,
}

/// Commitment to a block's system tx receipts, in execution order, over every field of the
/// receipts hl-node writes (the fields hyper-evm-sync compares):
/// `keccak256(rlp([[tx_type, success, cumulative_gas_used, logs], ...]))`, where
/// `cumulative_gas_used` counts the block's system txs only.
pub fn system_tx_receipts_hash<'a>(
    receipts: impl IntoIterator<Item = (u8, bool, u64, &'a [Log])>,
) -> B256 {
    #[derive(RlpEncodable)]
    struct Receipt {
        tx_type: u8,
        success: bool,
        cumulative_gas_used: u64,
        logs: Vec<Log>,
    }
    let receipts: Vec<Receipt> = receipts
        .into_iter()
        .map(|(tx_type, success, cumulative_gas_used, logs)| Receipt {
            tx_type,
            success,
            cumulative_gas_used,
            logs: logs.to_vec(),
        })
        .collect();
    keccak256(encode(&receipts))
}

impl LegacyReceipt {
    /// [`system_tx_receipts_hash`] of a block source's system tx receipts.
    ///
    /// Source receipts carry the cumulative gas of the system txs, which strictly increases as no
    /// system tx uses 0 gas. Receipts rebuilt from a nanoreth database carry 0 instead; then the
    /// outcome is unknown and `None` is returned (as for a block without system txs).
    pub(crate) fn system_tx_receipts_hash<'a>(
        receipts: impl IntoIterator<Item = &'a LegacyReceipt>,
    ) -> Option<B256> {
        let mut prev = 0;
        let mut fields = Vec::new();
        for r in receipts {
            if r.cumulative_gas_used <= prev {
                return None;
            }
            prev = r.cumulative_gas_used;
            fields.push((
                r.tx_type.clone() as u8,
                r.success,
                r.cumulative_gas_used,
                r.logs.as_slice(),
            ));
        }
        (!fields.is_empty()).then(|| system_tx_receipts_hash(fields))
    }
}

impl InMemorySize for HlExtras {
    fn size(&self) -> usize {
        self.read_precompile_calls.as_ref().map_or(0, |s| s.0.len())
            + self.highest_precompile_address.as_ref().map_or(0, |_| 20)
            + self.system_tx_receipts_hash.as_ref().map_or(0, |_| 32)
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
    /// [`HlBlockBody::system_tx_receipts_hash`](crate::HlBlockBody) of a block served by a
    /// nanoreth sync server (`rpc://`), whose stored system tx receipts no longer carry the gas
    /// to compute it from. Absent in hl-node blocks, which carry it in their receipts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_tx_receipts_hash: Option<B256>,
}

impl BlockAndReceipts {
    pub fn to_reth_block(self, chain_id: u64) -> HlBlock {
        let EvmBlock::Reth115(block) = self.block;
        let mut block = block.to_reth_block(
            self.read_precompile_calls,
            self.highest_precompile_address,
            self.system_txs,
            self.receipts,
            chain_id,
        );
        if block.body.system_tx_receipts_hash.is_none() {
            block.body.system_tx_receipts_hash = self.system_tx_receipts_hash;
        }
        block
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
            system_tx_receipts_hash: block.body.system_tx_receipts_hash,
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

#[cfg(test)]
mod system_tx_receipts_hash_tests {
    use super::*;

    fn receipt(success: bool, cumulative_gas_used: u64, logs: Vec<Log>) -> LegacyReceipt {
        LegacyReceipt { tx_type: LegacyTxType::Legacy, success, cumulative_gas_used, logs }
    }

    #[test]
    fn source_receipts_commit_to_every_field() {
        let log =
            Log::new_unchecked(Address::repeat_byte(1), vec![B256::repeat_byte(2)], Bytes::new());
        let source = [receipt(true, 45_071, vec![log.clone()]), receipt(false, 90_142, vec![])];
        let expected =
            system_tx_receipts_hash([(0, true, 45_071, &[log][..]), (0, false, 90_142, &[][..])]);
        assert_eq!(LegacyReceipt::system_tx_receipts_hash(&source), Some(expected));
    }

    #[test]
    fn unknown_gas_or_no_system_txs_commit_to_nothing() {
        // Receipts rebuilt from a nanoreth database carry no gas.
        assert_eq!(LegacyReceipt::system_tx_receipts_hash(&[receipt(true, 0, vec![])]), None);
        assert_eq!(LegacyReceipt::system_tx_receipts_hash(&[]), None);
    }

    #[test]
    fn any_field_change_changes_the_hash() {
        let base = system_tx_receipts_hash([(0, true, 21_000, &[][..])]);
        assert_ne!(base, system_tx_receipts_hash([(2, true, 21_000, &[][..])]));
        assert_ne!(base, system_tx_receipts_hash([(0, false, 21_000, &[][..])]));
        assert_ne!(base, system_tx_receipts_hash([(0, true, 21_001, &[][..])]));
        let log = Log::new_unchecked(Address::ZERO, vec![], Bytes::from_static(&[1]));
        assert_ne!(base, system_tx_receipts_hash([(0, true, 21_000, &[log][..])]));
    }
}
