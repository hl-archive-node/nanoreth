//! Copy of reth codebase to preserve serialization compatibility
use alloy_consensus::{Header, Signed, TxEip1559, TxEip2930, TxEip4844, TxEip7702, TxLegacy};
use alloy_primitives::{Address, BlockHash, Signature, TxKind, U256};
use reth_primitives::TransactionSigned as RethTxSigned;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, LazyLock, RwLock},
};
use tracing::info;

use crate::{
    node::{
        primitives::TransactionSigned as TxSigned,
        spot_meta::{erc20_contract_to_spot_token, SpotId},
        types::{ReadPrecompileCalls, SystemTx},
    },
    HlBlock, HlBlockBody,
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

fn system_tx_to_reth_transaction(transaction: &SystemTx, chain_id: u64) -> TxSigned {
    static EVM_MAP: LazyLock<Arc<RwLock<BTreeMap<Address, SpotId>>>> =
        LazyLock::new(|| Arc::new(RwLock::new(BTreeMap::new())));
    {
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
                if let Some(spot) = EVM_MAP.read().unwrap().get(&to) {
                    break spot.to_s();
                }

                info!("Contract not found: {to:?} from spot mapping, fetching again...");
                *EVM_MAP.write().unwrap() = erc20_contract_to_spot_token(chain_id).unwrap();
            }
        };
        let signature = Signature::new(U256::from(0x1), s, true);
        TxSigned::Default(RethTxSigned::Legacy(Signed::new_unhashed(tx.clone(), signature)))
    }
}

impl SealedBlock {
    pub fn to_reth_block(
        &self,
        read_precompile_calls: ReadPrecompileCalls,
        highest_precompile_address: Option<Address>,
        system_txs: Vec<super::SystemTx>,
        chain_id: u64,
    ) -> HlBlock {
        let mut merged_txs = vec![];
        merged_txs.extend(system_txs.iter().map(|tx| system_tx_to_reth_transaction(tx, chain_id)));
        merged_txs.extend(self.body.transactions.iter().map(|tx| tx.to_reth_transaction()));
        let block_body = HlBlockBody {
            inner: reth_primitives::BlockBody {
                transactions: merged_txs,
                withdrawals: self.body.withdrawals.clone(),
                ommers: self.body.ommers.clone(),
            },
            sidecars: None,
            read_precompile_calls: Some(read_precompile_calls),
            highest_precompile_address,
        };

        HlBlock { header: self.header.header.clone(), body: block_body }
    }
}
