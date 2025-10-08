use crate::node::primitives::TransactionSigned;
use alloy_consensus::Transaction as _;
use alloy_rpc_types::AccessList;
use auto_impl::auto_impl;
use reth_evm::{FromRecoveredTx, FromTxWithEncoded, IntoTxEnv, TransactionEnv};
use reth_primitives_traits::SignerRecoverable;
use revm::{
    context::TxEnv,
    context_interface::transaction::Transaction,
    primitives::{Address, Bytes, TxKind, B256, U256},
};

#[auto_impl(&, &mut, Box, Arc)]
pub trait HlTxTr: Transaction {}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct HlTxEnv<T: Transaction> {
    pub base: T,
}

impl<T: Transaction> HlTxEnv<T> {
    pub fn new(base: T) -> Self {
        Self { base }
    }
}

impl<T: Transaction> Transaction for HlTxEnv<T> {
    type AccessListItem<'a>
        = T::AccessListItem<'a>
    where
        T: 'a;
    type Authorization<'a>
        = T::Authorization<'a>
    where
        T: 'a;

    fn tx_type(&self) -> u8 {
        self.base.tx_type()
    }

    fn caller(&self) -> Address {
        self.base.caller()
    }

    fn gas_limit(&self) -> u64 {
        self.base.gas_limit()
    }

    fn value(&self) -> U256 {
        self.base.value()
    }

    fn input(&self) -> &Bytes {
        self.base.input()
    }

    fn nonce(&self) -> u64 {
        self.base.nonce()
    }

    fn kind(&self) -> TxKind {
        self.base.kind()
    }

    fn chain_id(&self) -> Option<u64> {
        self.base.chain_id()
    }

    fn gas_price(&self) -> u128 {
        self.base.gas_price()
    }

    fn access_list(&self) -> Option<impl Iterator<Item = Self::AccessListItem<'_>>> {
        self.base.access_list()
    }

    fn blob_versioned_hashes(&self) -> &[B256] {
        self.base.blob_versioned_hashes()
    }

    fn max_fee_per_blob_gas(&self) -> u128 {
        self.base.max_fee_per_blob_gas()
    }

    fn authorization_list_len(&self) -> usize {
        self.base.authorization_list_len()
    }

    fn authorization_list(&self) -> impl Iterator<Item = Self::Authorization<'_>> {
        self.base.authorization_list()
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.base.max_fee_per_gas()
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.base.max_priority_fee_per_gas()
    }

    fn effective_gas_price(&self, base_fee: u128) -> u128 {
        self.base.effective_gas_price(base_fee)
    }
}

impl<T: Transaction> HlTxTr for HlTxEnv<T> {}

impl<T: revm::context::Transaction> IntoTxEnv<Self> for HlTxEnv<T> {
    fn into_tx_env(self) -> Self {
        self
    }
}

impl FromRecoveredTx<TransactionSigned> for HlTxEnv<TxEnv> {
    fn from_recovered_tx(tx: &TransactionSigned, sender: Address) -> Self {
        if tx.gas_price().is_some() && tx.gas_price().unwrap() == 0 {
            return Self::new(TxEnv::from_recovered_tx(tx, tx.recover_signer().unwrap()));
        }

        Self::new(TxEnv::from_recovered_tx(tx, sender))
    }
}

impl FromTxWithEncoded<TransactionSigned> for HlTxEnv<TxEnv> {
    fn from_encoded_tx(tx: &TransactionSigned, sender: Address, _encoded: Bytes) -> Self {
        use reth_primitives::Transaction;
        let base = match tx.clone().into_inner().into_typed_transaction() {
            Transaction::Legacy(tx) => TxEnv::from_recovered_tx(&tx, sender),
            Transaction::Eip2930(tx) => TxEnv::from_recovered_tx(&tx, sender),
            Transaction::Eip1559(tx) => TxEnv::from_recovered_tx(&tx, sender),
            Transaction::Eip4844(tx) => TxEnv::from_recovered_tx(&tx, sender),
            Transaction::Eip7702(tx) => TxEnv::from_recovered_tx(&tx, sender),
        };

        Self { base }
    }
}

impl<T: TransactionEnv> TransactionEnv for HlTxEnv<T> {
    fn set_gas_limit(&mut self, gas_limit: u64) {
        self.base.set_gas_limit(gas_limit);
    }

    fn nonce(&self) -> u64 {
        TransactionEnv::nonce(&self.base)
    }

    fn set_nonce(&mut self, nonce: u64) {
        self.base.set_nonce(nonce);
    }

    fn set_access_list(&mut self, access_list: AccessList) {
        self.base.set_access_list(access_list);
    }
}
