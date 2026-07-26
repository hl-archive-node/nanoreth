//! HlNodePrimitives::TransactionSigned; it's the same as ethereum transaction type,
//! except that it supports pseudo signer for system transactions.
use std::convert::Infallible;

use crate::evm::transaction::HlTxEnv;
use alloy_consensus::{
    SignableTransaction, Signed, Transaction as TransactionTrait, TransactionEnvelope, TxEip1559,
    TxEip2930, TxEip4844, TxEip7702, TxLegacy, TxType, TypedTransaction, crypto::RecoveryError,
    error::ValueError, transaction::TxHashRef,
};
use alloy_eips::Encodable2718;
use alloy_network::TxSigner;
use alloy_primitives::{Address, TxHash, U256, address, uint};
use alloy_rpc_types::{Transaction, TransactionInfo, TransactionRequest};
use alloy_signer::Signature;
use reth_codecs::alloy::transaction::{Envelope, FromTxCompact};
use reth_codecs::DecompressError;
use reth_db::table::{Compress, Decompress};
use reth_ethereum_primitives::PooledTransactionVariant;
use reth_evm::FromRecoveredTx;
use reth_primitives_traits::Recovered;
use reth_primitives_traits::{InMemorySize, SignerRecoverable};
use reth_rpc_convert::{FromConsensusTx, TryIntoTxEnv};
use reth_rpc_eth_api::{EthTxEnvError, SignTxRequestError, SignableTxRequest, TryIntoSimTx};
use crate::evm::spec::HlSpecId;
use reth_evm::EvmEnv;
use revm::context::{BlockEnv, TxEnv};

type InnerType = alloy_consensus::EthereumTxEnvelope<TxEip4844>;

#[derive(Debug, Clone, TransactionEnvelope)]
#[envelope(tx_type_name = HlTxType)]
pub enum TransactionSigned {
    #[envelope(flatten)]
    Default(InnerType),
}

/// The HYPE system address (sender of native HYPE transfer system txs), encoded as `s == 1`.
/// <https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/hyperevm/hypercore-less-than-greater-than-hyperevm-transfers>
const HYPE_SYSTEM_ADDRESS: Address = address!("2222222222222222222222222222222222222222");

/// 0x00..01: its natural encoding `s == 1` would collide with [`HYPE_SYSTEM_ADDRESS`], so it is
/// escaped to [`ADDRESS_ONE_S`].
const ADDRESS_ONE: Address = address!("0000000000000000000000000000000000000001");

/// The escaped `s` for [`ADDRESS_ONE`]: `(1 << 160) | 1`, one bit above the 20-byte address space.
const ADDRESS_ONE_S: U256 = uint!(0x10000000000000000000000000000000000000001_U256);

/// Decode a system tx's synthetic signature `s` into `msg.sender`; inverse of [`address_to_s`].
fn s_to_address(s: U256) -> Address {
    if s == U256::ONE {
        return HYPE_SYSTEM_ADDRESS;
    }
    if s == ADDRESS_ONE_S {
        return ADDRESS_ONE;
    }
    Address::from_slice(&s.to_be_bytes::<32>()[12..])
}

/// Encode `msg.sender` into a system tx's synthetic signature `s`; inverse of [`s_to_address`].
pub(crate) fn address_to_s(addr: Address) -> U256 {
    if addr == HYPE_SYSTEM_ADDRESS {
        return U256::ONE;
    }
    if addr == ADDRESS_ONE {
        return ADDRESS_ONE_S;
    }
    U256::from_be_slice(addr.as_slice())
}

impl TxHashRef for TransactionSigned {
    fn tx_hash(&self) -> &TxHash {
        self.inner().tx_hash()
    }
}

impl SignerRecoverable for TransactionSigned {
    fn recover_signer(&self) -> Result<Address, RecoveryError> {
        if self.is_system_transaction() {
            return Ok(s_to_address(self.signature().s()));
        }
        self.inner().recover_signer()
    }

    fn recover_signer_unchecked(&self) -> Result<Address, RecoveryError> {
        if self.is_system_transaction() {
            return Ok(s_to_address(self.signature().s()));
        }
        self.inner().recover_signer_unchecked()
    }

    fn recover_unchecked_with_buf(&self, buf: &mut Vec<u8>) -> Result<Address, RecoveryError> {
        if self.is_system_transaction() {
            return Ok(s_to_address(self.signature().s()));
        }
        self.inner().recover_unchecked_with_buf(buf)
    }
}

// ------------------------------------------------------------
// NOTE: All lines below are just wrappers for the inner type.
// ------------------------------------------------------------

macro_rules! impl_from_signed {
    ($($tx:ident),*) => {
        $(
            impl From<Signed<$tx>> for TransactionSigned {
                fn from(value: Signed<$tx>) -> Self {
                    Self::Default(value.into())
                }
            }
        )*
    };
}

impl_from_signed!(TxLegacy, TxEip2930, TxEip1559, TxEip7702, TypedTransaction);

impl InMemorySize for TransactionSigned {
    #[inline]
    fn size(&self) -> usize {
        self.inner().size()
    }
}

impl reth_codecs::Compact for TransactionSigned {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        self.inner().to_compact(buf)
    }

    fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
        let (tx, hash) = InnerType::from_compact(buf, _len);
        (Self::Default(tx), hash)
    }
}

impl FromRecoveredTx<TransactionSigned> for TxEnv {
    fn from_recovered_tx(tx: &TransactionSigned, sender: Address) -> Self {
        TxEnv::from_recovered_tx(&tx.inner(), sender)
    }
}

impl FromTxCompact for TransactionSigned {
    type TxType = TxType;

    fn from_tx_compact(buf: &[u8], tx_type: Self::TxType, signature: Signature) -> (Self, &[u8])
    where
        Self: Sized,
    {
        let (tx, buf) = InnerType::from_tx_compact(buf, tx_type, signature);
        (Self::Default(tx), buf)
    }
}

impl reth_codecs::alloy::transaction::Envelope for TransactionSigned {
    fn signature(&self) -> &Signature {
        self.inner().signature()
    }

    fn tx_type(&self) -> Self::TxType {
        self.inner().tx_type()
    }
}

impl TransactionSigned {
    #[inline]
    pub fn into_inner(self) -> InnerType {
        match self {
            Self::Default(tx) => tx,
        }
    }

    #[inline]
    pub const fn inner(&self) -> &InnerType {
        match self {
            Self::Default(tx) => tx,
        }
    }

    pub fn is_system_transaction(&self) -> bool {
        matches!(self.gas_price(), Some(0))
    }
}

impl TryFrom<TransactionSigned> for PooledTransactionVariant {
    type Error = <InnerType as TryInto<PooledTransactionVariant>>::Error;

    fn try_from(value: TransactionSigned) -> Result<Self, Self::Error> {
        value.into_inner().try_into()
    }
}

impl From<PooledTransactionVariant> for TransactionSigned {
    fn from(value: PooledTransactionVariant) -> Self {
        Self::Default(value.into())
    }
}

impl Compress for TransactionSigned {
    type Compressed = Vec<u8>;

    fn compress_to_buf<B: bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
        self.inner().compress_to_buf(buf);
    }
}

impl Decompress for TransactionSigned {
    fn decompress(value: &[u8]) -> Result<Self, DecompressError> {
        Ok(Self::Default(InnerType::decompress(value)?))
    }
}

impl TryIntoSimTx<TransactionSigned> for TransactionRequest {
    fn try_into_sim_tx(self) -> Result<TransactionSigned, ValueError<Self>> {
        let tx = self
            .build_typed_tx()
            .map_err(|request| ValueError::new(request, "Required fields missing"))?;

        // Create an empty signature for the transaction.
        let signature = Signature::new(Default::default(), Default::default(), false);

        Ok(tx.into_signed(signature).into())
    }
}

impl TryIntoTxEnv<HlTxEnv<TxEnv>, HlSpecId, BlockEnv> for TransactionRequest {
    type Err = EthTxEnvError;

    fn try_into_tx_env(
        self,
        evm_env: &EvmEnv<HlSpecId, BlockEnv>,
    ) -> Result<HlTxEnv<TxEnv>, Self::Err> {
        // The inner ethereum conversion works off the mapped ethereum spec.
        let eth_env = EvmEnv {
            cfg_env: evm_env
                .cfg_env
                .clone()
                .with_spec_and_mainnet_gas_params(evm_env.cfg_env.spec.into_eth_spec()),
            block_env: evm_env.block_env.clone(),
        };
        Ok(HlTxEnv::new(self.clone().try_into_tx_env(&eth_env)?))
    }
}

impl FromConsensusTx<TransactionSigned> for Transaction {
    type TxInfo = TransactionInfo;
    type Err = Infallible;

    fn from_consensus_tx(
        tx: TransactionSigned,
        signer: Address,
        tx_info: Self::TxInfo,
    ) -> Result<Self, Self::Err> {
        Ok(Self::from_transaction(
            Recovered::new_unchecked(tx.into_inner().into(), signer),
            tx_info,
        ))
    }
}

impl SignableTxRequest<TransactionSigned> for TransactionRequest {
    async fn try_build_and_sign(
        self,
        signer: impl TxSigner<Signature> + Send,
    ) -> Result<TransactionSigned, SignTxRequestError> {
        let signed = SignableTxRequest::<InnerType>::try_build_and_sign(self, signer).await?;
        Ok(TransactionSigned::Default(signed))
    }
}
