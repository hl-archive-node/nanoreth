//! Copy of reth codebase to preserve serialization compatibility
use crate::node::storage::tables::{SPOT_METADATA_KEY, SpotMetadata};
use alloy_consensus::{Header, Signed, TxEip1559, TxEip2930, TxEip4844, TxEip7702, TxLegacy};
use alloy_primitives::{Address, BlockHash, Bytes, Signature, TxKind, U256};
use reth_db::{DatabaseEnv, DatabaseError, cursor::DbCursorRW};
use reth_db_api::{Database, transaction::DbTxMut};
use reth_primitives::TransactionSigned as RethTxSigned;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, LazyLock, Mutex, RwLock},
};
use tracing::info;

use crate::{
    HlBlock, HlBlockBody, HlHeader,
    node::{
        primitives::TransactionSigned as TxSigned,
        spot_meta::{SpotId, erc20_contract_to_spot_token},
        types::{LegacyReceipt, ReadPrecompileCalls, SystemTx},
    },
};

/// A raw transaction.
///
/// Transaction types were introduced in [EIP-2718](https://eips.ethereum.org/EIPS/eip-2718).
#[derive(Debug, Clone, PartialEq, Eq, Hash, derive_more::From, Serialize, Deserialize)]
pub enum Transaction {
    Legacy(TxLegacy),
    Eip2930(TxEip2930),
    Eip1559(TxEip1559),
    Eip4844(TxEip4844),
    Eip7702(TxEip7702),
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
                Self { signature: sig, transaction: Transaction::Legacy(tx) }
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
            EthereumTxEnvelope::Legacy(signed) => Transaction::Legacy(signed.into_parts().0),
            EthereumTxEnvelope::Eip2930(signed) => Transaction::Eip2930(signed.into_parts().0),
            EthereumTxEnvelope::Eip1559(signed) => Transaction::Eip1559(signed.into_parts().0),
            EthereumTxEnvelope::Eip4844(signed) => Transaction::Eip4844(signed.into_parts().0),
            EthereumTxEnvelope::Eip7702(signed) => Transaction::Eip7702(signed.into_parts().0),
        }
    }

    fn to_reth_transaction(&self) -> TxSigned {
        match self.transaction.clone() {
            Transaction::Legacy(tx) => {
                TxSigned::Default(RethTxSigned::Legacy(Signed::new_unhashed(tx, self.signature)))
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

fn system_tx_to_reth_transaction(transaction: &SystemTx, chain_id: u64) -> TxSigned {
    let Transaction::Legacy(tx) = &transaction.tx else {
        panic!("Unexpected transaction type");
    };
    let TxKind::Call(to) = tx.to else {
        panic!("Unexpected contract creation");
    };
    let s = if tx.input.is_empty() {
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
    TxSigned::Default(RethTxSigned::Legacy(Signed::new_unhashed(tx.clone(), signature)))
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
        merged_txs.extend(system_txs.iter().map(|tx| system_tx_to_reth_transaction(tx, chain_id)));
        merged_txs.extend(self.body.transactions.iter().map(|tx| tx.to_reth_transaction()));

        let mut merged_receipts = vec![];
        merged_receipts.extend(system_txs.iter().map(|tx| tx.receipt.clone().unwrap().into()));
        merged_receipts.extend(receipts.into_iter().map(From::from));

        let block_body = HlBlockBody {
            inner: reth_primitives::BlockBody {
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
