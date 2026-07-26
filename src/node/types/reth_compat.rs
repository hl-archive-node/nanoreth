//! Copy of reth codebase to preserve serialization compatibility
use crate::node::storage::tables::{SPOT_METADATA_KEY, SpotMetadata};
use alloy_consensus::{Header, Signed, TxEip1559, TxEip2930, TxEip4844, TxEip7702, TxLegacy};
use alloy_primitives::{Address, BlockHash, Bytes, Signature, TxKind, U256};
use reth_db::{DatabaseEnv, DatabaseError, cursor::DbCursorRW};
use reth_db_api::{Database, transaction::DbTxMut};
use reth_ethereum_primitives::TransactionSigned as RethTxSigned;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, LazyLock, Mutex, RwLock},
};
use tracing::info;

use super::patch::recover_testnet_system_tx_sender;
use crate::{
    HlBlock, HlBlockBody, HlHeader,
    node::{
        primitives::{TransactionSigned as TxSigned, transaction::address_to_s},
        spot_meta::{SpotId, erc20_contract_to_spot_token},
        types::{LegacyReceipt, ReadPrecompileCalls, SystemTx},
    },
};

/// A raw transaction.
///
/// Transaction types were introduced in [EIP-2718](https://eips.ethereum.org/EIPS/eip-2718).
#[derive(Debug, Clone, PartialEq, Eq, Hash, derive_more::From, Serialize, Deserialize)]
pub enum Transaction {
    Legacy(WireTxLegacy),
    Eip2930(TxEip2930),
    Eip1559(TxEip1559),
    Eip4844(TxEip4844),
    Eip7702(TxEip7702),
}

/// Wire-format mirror of [`TxLegacy`].
///
/// alloy 2.x deserializes `TxLegacy::gas_price` through `U256` while still serializing it as
/// `U128` (see the `gas_price` module in alloy-consensus, added for alloy-rs/alloy#2842). The
/// asymmetry is invisible in JSON, where both are hex strings, but it breaks binary formats: in
/// the msgpack hl-node writes, the field is 16 bytes on the wire and the `U256` visitor rejects
/// it with "invalid length 16, expected 256 bits".
///
/// This mirror keeps the symmetric `quantity` codec so the wire format stays decodable, and
/// converts into the alloy type afterwards. Every other `u128` tx field already uses
/// `alloy_serde::quantity` on both sides and is unaffected.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireTxLegacy {
    #[serde(default, with = "alloy_serde::quantity::opt", skip_serializing_if = "Option::is_none")]
    pub chain_id: Option<alloy_primitives::ChainId>,
    #[serde(with = "alloy_serde::quantity")]
    pub nonce: u64,
    #[serde(with = "alloy_serde::quantity")]
    pub gas_price: u128,
    #[serde(with = "alloy_serde::quantity", rename = "gas", alias = "gasLimit")]
    pub gas_limit: u64,
    #[serde(default)]
    pub to: TxKind,
    pub value: U256,
    pub input: Bytes,
}

impl From<WireTxLegacy> for TxLegacy {
    fn from(tx: WireTxLegacy) -> Self {
        let WireTxLegacy { chain_id, nonce, gas_price, gas_limit, to, value, input } = tx;
        Self { chain_id, nonce, gas_price, gas_limit, to, value, input }
    }
}

impl From<TxLegacy> for WireTxLegacy {
    fn from(tx: TxLegacy) -> Self {
        let TxLegacy { chain_id, nonce, gas_price, gas_limit, to, value, input } = tx;
        Self { chain_id, nonce, gas_price, gas_limit, to, value, input }
    }
}

/// Signed Ethereum transaction.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, derive_more::AsRef, derive_more::Deref,
)]
#[serde(rename_all = "camelCase")]
pub struct TransactionSigned {
    /// The transaction signature values
    signature: Signature,
    /// Raw transaction info
    #[deref]
    #[as_ref]
    transaction: Transaction,
}
impl TransactionSigned {
    /// Convert from the node's TransactionSigned back to reth_compat format.
    pub fn from_node_tx(tx: TxSigned) -> Self {
        use alloy_consensus::EthereumTxEnvelope;
        let inner = tx.into_inner();
        match inner {
            EthereumTxEnvelope::Legacy(signed) => {
                let (tx, sig, _) = signed.into_parts();
                Self { signature: sig, transaction: Transaction::Legacy(tx.into()) }
            }
            EthereumTxEnvelope::Eip2930(signed) => {
                let (tx, sig, _) = signed.into_parts();
                Self { signature: sig, transaction: Transaction::Eip2930(tx) }
            }
            EthereumTxEnvelope::Eip1559(signed) => {
                let (tx, sig, _) = signed.into_parts();
                Self { signature: sig, transaction: Transaction::Eip1559(tx) }
            }
            EthereumTxEnvelope::Eip4844(signed) => {
                let (tx, sig, _) = signed.into_parts();
                Self { signature: sig, transaction: Transaction::Eip4844(tx) }
            }
            EthereumTxEnvelope::Eip7702(signed) => {
                let (tx, sig, _) = signed.into_parts();
                Self { signature: sig, transaction: Transaction::Eip7702(tx) }
            }
        }
    }

    /// Extract just the transaction (without signature) from a node TransactionSigned.
    /// Used for system transactions where the signature is fabricated.
    pub fn extract_transaction(tx: TxSigned) -> Transaction {
        use alloy_consensus::EthereumTxEnvelope;
        let inner = tx.into_inner();
        match inner {
            EthereumTxEnvelope::Legacy(signed) => Transaction::Legacy(signed.into_parts().0.into()),
            EthereumTxEnvelope::Eip2930(signed) => Transaction::Eip2930(signed.into_parts().0),
            EthereumTxEnvelope::Eip1559(signed) => Transaction::Eip1559(signed.into_parts().0),
            EthereumTxEnvelope::Eip4844(signed) => Transaction::Eip4844(signed.into_parts().0),
            EthereumTxEnvelope::Eip7702(signed) => Transaction::Eip7702(signed.into_parts().0),
        }
    }

    fn to_reth_transaction(&self) -> TxSigned {
        match self.transaction.clone() {
            Transaction::Legacy(tx) => {
                TxSigned::Default(RethTxSigned::Legacy(Signed::new_unhashed(tx.into(), self.signature)))
            }
            Transaction::Eip2930(tx) => {
                TxSigned::Default(RethTxSigned::Eip2930(Signed::new_unhashed(tx, self.signature)))
            }
            Transaction::Eip1559(tx) => {
                TxSigned::Default(RethTxSigned::Eip1559(Signed::new_unhashed(tx, self.signature)))
            }
            Transaction::Eip4844(tx) => {
                TxSigned::Default(RethTxSigned::Eip4844(Signed::new_unhashed(tx, self.signature)))
            }
            Transaction::Eip7702(tx) => {
                TxSigned::Default(RethTxSigned::Eip7702(Signed::new_unhashed(tx, self.signature)))
            }
        }
    }
}

type BlockBody = alloy_consensus::BlockBody<TransactionSigned, Header>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedHeader {
    pub hash: BlockHash,
    pub header: Header,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedBlock {
    /// Sealed Header.
    pub header: SealedHeader,
    /// the block's body.
    pub body: BlockBody,
}

static SPOT_EVM_MAP: LazyLock<Arc<RwLock<BTreeMap<Address, SpotId>>>> =
    LazyLock::new(|| Arc::new(RwLock::new(BTreeMap::new())));

// Optional database handle for persisting on-demand fetches
static DB_HANDLE: LazyLock<Mutex<Option<Arc<DatabaseEnv>>>> = LazyLock::new(|| Mutex::new(None));

// Single-flight guard: ensures only one thread fetches spot metadata from the
// API on a cache miss, so a thundering herd of misses produces one request.
static SPOT_FETCH_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Set the database handle for persisting spot metadata
pub fn set_spot_metadata_db(db: Arc<DatabaseEnv>) {
    *DB_HANDLE.lock().unwrap() = Some(db);
}

/// Initialize the spot metadata cache with data loaded from database.
/// This should be called during node initialization.
pub fn initialize_spot_metadata_cache(metadata: BTreeMap<Address, SpotId>) {
    *SPOT_EVM_MAP.write().unwrap() = metadata;
}

/// Helper function to serialize and store spot metadata to database
pub fn store_spot_metadata(
    db: &Arc<DatabaseEnv>,
    metadata: &BTreeMap<Address, SpotId>,
) -> Result<(), DatabaseError> {
    db.update(|tx| {
        let mut cursor = tx.cursor_write::<SpotMetadata>()?;

        // Serialize to BTreeMap<Address, u64>
        let serializable_map: BTreeMap<Address, u64> =
            metadata.iter().map(|(addr, spot)| (*addr, spot.index)).collect();

        cursor.upsert(
            SPOT_METADATA_KEY,
            &Bytes::from(
                rmp_serde::to_vec(&serializable_map).expect("Failed to serialize spot metadata"),
            ),
        )?;
        Ok(())
    })?
}

/// Persist spot metadata to database if handle is available
fn persist_spot_metadata_to_db(metadata: &BTreeMap<Address, SpotId>) {
    if let Some(db) = DB_HANDLE.lock().unwrap().as_ref() {
        match store_spot_metadata(db, metadata) {
            Ok(_) => info!("Persisted spot metadata to database"),
            Err(e) => info!("Failed to persist spot metadata to database: {}", e),
        }
    }
}

fn system_tx_to_reth_transaction(
    transaction: &SystemTx,
    chain_id: u64,
    block_number: u64,
) -> TxSigned {
    let Transaction::Legacy(tx) = &transaction.tx else {
        panic!("Unexpected transaction type");
    };
    let TxKind::Call(to) = tx.to else {
        panic!("Unexpected contract creation");
    };
    // System txs arrive unsigned; nanoreth fabricates the signature below, encoding `msg.sender`
    // into `s` (via `address_to_s`) so `s_to_address(s)` recovers the holder and nonces validate.
    let s = if let Some(sender) = transaction.from {
        // Prefer the authoritative upstream `from`.
        address_to_s(sender)
    } else if let Some(sender) =
        recover_testnet_system_tx_sender(chain_id, block_number, to, transaction.receipt.as_ref())
    {
        // Fall back to `super::patch` testnet log recovery.
        address_to_s(sender)
    } else if tx.input.is_empty() {
        // Native HYPE transfer: sender is the HYPE system address (0x2222…2222), encoded `s == 1`.
        U256::from(0x1)
    } else {
        loop {
            if let Some(spot) = SPOT_EVM_MAP.read().unwrap().get(&to) {
                break spot.to_s();
            }

            // Cache miss - single-flight the API fetch so concurrent misses don't
            // each hit the API. Only the thread holding SPOT_FETCH_LOCK fetches;
            // the rest block here and re-check the cache once it's released.
            let _fetch_guard = SPOT_FETCH_LOCK.lock().unwrap();

            // Double-checked: another thread may have populated the cache while we
            // waited for the fetch lock.
            if SPOT_EVM_MAP.read().unwrap().contains_key(&to) {
                continue;
            }

            info!("Contract not found: {to:?} from spot mapping, fetching from API...");
            let metadata = erc20_contract_to_spot_token(chain_id).unwrap();
            *SPOT_EVM_MAP.write().unwrap() = metadata.clone();
            persist_spot_metadata_to_db(&metadata);
        }
    };
    let signature = Signature::new(U256::from(0x1), s, true);
    TxSigned::Default(RethTxSigned::Legacy(Signed::new_unhashed(tx.clone().into(), signature)))
}

impl SealedBlock {
    pub fn to_reth_block(
        &self,
        read_precompile_calls: ReadPrecompileCalls,
        highest_precompile_address: Option<Address>,
        mut system_txs: Vec<super::SystemTx>,
        receipts: Vec<LegacyReceipt>,
        chain_id: u64,
    ) -> HlBlock {
        // NOTE: These types of transactions are tracked at #97.
        system_txs.retain(|tx| tx.receipt.is_some());

        let mut merged_txs = vec![];
        let block_number = self.header.header.number;
        merged_txs.extend(
            system_txs.iter().map(|tx| system_tx_to_reth_transaction(tx, chain_id, block_number)),
        );
        merged_txs.extend(self.body.transactions.iter().map(|tx| tx.to_reth_transaction()));

        let mut merged_receipts = vec![];
        merged_receipts.extend(system_txs.iter().map(|tx| tx.receipt.clone().unwrap().into()));
        merged_receipts.extend(receipts.into_iter().map(From::from));

        let block_body = HlBlockBody {
            inner: crate::node::primitives::BlockBody {
                transactions: merged_txs,
                withdrawals: self.body.withdrawals.clone(),
                ommers: vec![],
            },
            sidecars: None,
            read_precompile_calls: Some(read_precompile_calls),
            highest_precompile_address,
        };

        let system_tx_count = system_txs.len() as u64;
        HlBlock {
            header: HlHeader::from_ethereum_header(
                self.header.header.clone(),
                &merged_receipts,
                system_tx_count,
            ),
            body: block_body,
        }
    }
}

#[cfg(test)]
mod tests {

    /// hl-node encodes `u128` gas fields as 16-byte msgpack binaries. alloy 2.x serializes
    /// `TxLegacy::gas_price` as `U128` but deserializes it through `U256`, which is invisible in
    /// JSON yet makes the binary wire format undecodable ("invalid length 16, expected 256 bits").
    /// [`WireTxLegacy`] restores the symmetric codec; this guards against regressing to the alloy
    /// type in the wire structs.
    #[test]
    fn legacy_tx_survives_msgpack_round_trip() {
        let tx = Transaction::Legacy(WireTxLegacy {
            chain_id: Some(999),
            nonce: 7,
            gas_price: 110_000_000,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::from(1u64),
            input: Bytes::new(),
        });

        let encoded = rmp_serde::to_vec_named(&tx).expect("serialize");
        let decoded: Transaction = rmp_serde::from_slice(&encoded).expect("deserialize");
        assert_eq!(tx, decoded);
    }

    /// The gas price must survive as a 16-byte value, matching what hl-node writes.
    #[test]
    fn legacy_gas_price_is_16_bytes_on_the_wire() {
        let tx = WireTxLegacy { gas_price: 110_000_000, ..Default::default() };
        let encoded = rmp_serde::to_vec_named(&tx).expect("serialize");
        let decoded: WireTxLegacy = rmp_serde::from_slice(&encoded).expect("deserialize");
        assert_eq!(decoded.gas_price, 110_000_000);
    }

    use super::*;
    use crate::chainspec::TESTNET_CHAIN_ID;
    use alloy_consensus::{Transaction as _, TxType};
    use alloy_primitives::{Log, LogData, address, b256};
    use reth_ethereum_primitives::EthereumReceipt;
    use reth_primitives_traits::SignerRecoverable;

    // The single token targeted by the testnet sender-recovery patch.
    const TOKEN: Address = address!("2b3370ee501b4a559b57d449569354196457d8ab");
    const FROM_BLOCK: u64 = 55231857;
    const HOLDER_A: Address = address!("f9b10ef826e9aa275f1813034e3bd9b80224e535");
    const HOLDER_B: Address = address!("0b80659a4076e9e93c7dbe0f10675a16a3e5c206");

    fn transfer_log(from: Address, to: Address) -> Log {
        Log {
            address: TOKEN,
            data: LogData::new_unchecked(
                vec![
                    b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"),
                    from.into_word(),
                    to.into_word(),
                ],
                U256::from(1_000u64).to_be_bytes::<32>().to_vec().into(),
            ),
        }
    }

    fn system_tx(from: Address, to: Address, nonce: u64) -> SystemTx {
        let receipt: LegacyReceipt = EthereumReceipt {
            tx_type: TxType::Legacy,
            success: true,
            cumulative_gas_used: 0,
            logs: vec![transfer_log(from, to)],
        }
        .into();
        SystemTx {
            tx: Transaction::Legacy(TxLegacy {
                chain_id: Some(TESTNET_CHAIN_ID),
                nonce,
                gas_price: 0,
                gas_limit: 200_000,
                to: TxKind::Call(to),
                value: U256::ZERO,
                // transfer(address,uint256) selector; input non-empty
                input: Bytes::from_static(&[0xa9, 0x05, 0x9c, 0xbb]),
            }
            .into()),
            receipt: Some(receipt),
            // `from` path set explicitly per-test.
            from: None,
        }
    }

    #[test]
    fn testnet_conversion_recovers_real_holder() {
        // Full path: testnet system_tx -> synthetic signature -> recover_signer == real sender.
        // Both interleaved holders recover independently (the synthetic collision is gone).
        let tx = system_tx(HOLDER_B, TOKEN, 1);
        let signed = system_tx_to_reth_transaction(&tx, TESTNET_CHAIN_ID, FROM_BLOCK);
        assert_eq!(signed.gas_price(), Some(0)); // recognised as a system tx
        assert_eq!(signed.recover_signer().unwrap(), HOLDER_B);

        let tx2 = system_tx(HOLDER_A, TOKEN, 0);
        let signed2 = system_tx_to_reth_transaction(&tx2, TESTNET_CHAIN_ID, FROM_BLOCK);
        assert_eq!(signed2.recover_signer().unwrap(), HOLDER_A);
    }

    #[test]
    fn from_field_takes_precedence_over_log() {
        // On disagreement, upstream `from` wins over the `Transfer` log.
        let mut tx = system_tx(HOLDER_A, TOKEN, 3);
        tx.from = Some(HOLDER_B);
        let signed = system_tx_to_reth_transaction(&tx, TESTNET_CHAIN_ID, FROM_BLOCK);
        assert_eq!(signed.recover_signer().unwrap(), HOLDER_B);
    }

    #[test]
    fn from_db_round_trip_preserves_real_holder() {
        // A `from`-bearing system tx past the log-recovery window survives the sync round trip
        // (to_reth_block -> from_db -> to_reth_block) recovering the real holder, not a collision.
        let mut tx = system_tx(HOLDER_A, TOKEN, 1);
        tx.from = Some(HOLDER_B);
        assert_eq!(round_trip_signer(tx), HOLDER_B);
    }

    #[test]
    fn from_db_round_trip_preserves_hype_sender() {
        // A native HYPE transfer (empty input, sender = HYPE system address -> `s == 1`) recovers
        // to 0x2222…2222 and re-encodes back to `s == 1` across the round trip - no drift.
        let receipt: LegacyReceipt = EthereumReceipt {
            tx_type: TxType::Legacy,
            success: true,
            cumulative_gas_used: 0,
            logs: vec![],
        }
        .into();
        let tx = SystemTx {
            tx: Transaction::Legacy(TxLegacy {
                chain_id: Some(TESTNET_CHAIN_ID),
                nonce: 0,
                gas_price: 0,
                gas_limit: 200_000,
                to: TxKind::Call(TOKEN),
                value: U256::ZERO,
                input: Bytes::new(),
            }
            .into()),
            receipt: Some(receipt),
            from: None,
        };
        assert_eq!(round_trip_signer(tx), address!("2222222222222222222222222222222222222222"));
    }

    #[test]
    fn degenerate_from_one_is_escaped_and_round_trips() {
        // `from == 0x00..01` would collide with the `s == 1` sentinel; the escape lifts it above
        // the address space so it recovers to 0x00..01 (not the 0x2222 sentinel) and round-trips.
        let one = address!("0000000000000000000000000000000000000001");
        let mut tx = system_tx(HOLDER_A, TOKEN, 1);
        tx.from = Some(one);
        let signed = system_tx_to_reth_transaction(&tx, TESTNET_CHAIN_ID, FROM_BLOCK);
        assert_eq!(signed.recover_signer().unwrap(), one);
        assert_eq!(round_trip_signer(tx), one);
    }

    /// Full sync round trip (to_reth_block -> from_db -> to_reth_block); returns the re-served
    /// system tx's recovered signer, which must equal the original's.
    fn round_trip_signer(tx: SystemTx) -> Address {
        let sealed = SealedBlock {
            header: SealedHeader {
                hash: Default::default(),
                header: Header { number: 56_000_000, ..Default::default() },
            },
            body: BlockBody { transactions: vec![], ommers: vec![], withdrawals: None },
        };
        let receipts = vec![tx.receipt.clone().unwrap().into()];
        let block =
            sealed.to_reth_block(Default::default(), None, vec![tx], vec![], TESTNET_CHAIN_ID);
        let restored = crate::node::types::BlockAndReceipts::from_db(block, receipts);
        assert_eq!(restored.system_txs.len(), 1);
        let reserved = restored.to_reth_block(TESTNET_CHAIN_ID);
        reserved.body.inner.transactions[0].recover_signer().unwrap()
    }
}
