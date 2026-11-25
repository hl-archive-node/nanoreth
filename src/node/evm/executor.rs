use super::{
    config::HlBlockExecutionCtx, patch::patch_mainnet_after_tx,
    read_precompile_forwarder::ReadPrecompileForwarder,
};
use crate::{
    evm::transaction::HlTxEnv,
    hardforks::HlHardforks,
    node::{
        primitives::{HlTxType, TransactionSigned},
        types::{HlExtras, ReadPrecompileInput, ReadPrecompileResult, system_tx_receipts_hash},
    },
};
use alloy_consensus::{Transaction, TransactionEnvelope, TxReceipt};
use alloy_eips::{Encodable2718, Typed2718, eip7685::Requests};
use alloy_evm::{
    RecoveredTx,
    block::{ExecutableTx, GasOutput, TxResult},
    eth::receipt_builder::ReceiptBuilderCtx,
};
use alloy_primitives::{Address, B256, Bytes, Log, U160, U256, address, hex};
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use reth_evm::{
    Evm, FromRecoveredTx, FromTxWithEncoded, IntoTxEnv,
    block::{BlockValidationError, StateDB},
    eth::receipt_builder::ReceiptBuilder,
    execute::{BlockExecutionError, BlockExecutor},
    precompiles::{DynPrecompile, PrecompileInput, PrecompilesMap},
};
use reth_provider::BlockExecutionResult;
use revm::{
    Database, DatabaseCommit,
    context::Block as _,
    context::{TxEnv, result::ResultAndState},
    interpreter::instructions::utility::IntoU256,
    precompile::{PrecompileHalt, PrecompileOutput, PrecompileResult},
    primitives::HashMap,
    state::{Account, Bytecode},
};
use std::sync::Arc;
use tracing::warn;

pub type ScopedReadPrecompileForwarder = (Arc<ReadPrecompileForwarder>, B256);

pub fn is_system_transaction(tx: &TransactionSigned) -> bool {
    let Some(gas_price) = tx.gas_price() else {
        return false;
    };
    gas_price == 0
}

/// Per-transaction execution result produced by [`HlBlockExecutor`].
///
/// Carries the HL-specific bits that `commit_transaction` needs, since it no longer receives the
/// transaction itself.
#[derive(Debug)]
pub struct HlTxResult<H> {
    /// Result of the transaction execution.
    pub result: ResultAndState<H>,
    /// Type of the transaction.
    pub tx_type: HlTxType,
    /// Whether this was a HyperCore system transaction.
    pub is_system: bool,
}

impl<H: Send + 'static> TxResult for HlTxResult<H> {
    type HaltReason = H;

    fn result(&self) -> &ResultAndState<Self::HaltReason> {
        &self.result
    }

    fn into_result(self) -> ResultAndState<Self::HaltReason> {
        self.result
    }
}

pub struct HlBlockExecutor<'a, EVM, Spec, R: ReceiptBuilder>
where
    Spec: EthChainSpec,
{
    /// Reference to the specification object.
    #[allow(dead_code)]
    spec: Spec,
    /// Inner EVM.
    evm: EVM,
    /// Gas used in the block.
    gas_used: u64,
    /// Receipts of executed transactions.
    receipts: Vec<R::Receipt>,
    /// Receipt builder.
    receipt_builder: R,
    /// Context for block execution.
    #[allow(dead_code)]
    ctx: HlBlockExecutionCtx<'a>,
    /// Receipt fields `(tx_type, success, cumulative_gas_used, logs)` of each executed system tx,
    /// with the cumulative gas of the system txs only, recorded only when there is a commitment to
    /// check them against.
    system_tx_receipts: Vec<(u8, bool, u64, Vec<Log>)>,
}

/// Replays a read precompile call from the calls recorded in the block body, or returns [`None`]
/// if the block did not record this exact input.
fn run_precompile(
    precompile_calls: &HashMap<ReadPrecompileInput, ReadPrecompileResult>,
    data: &[u8],
    gas_limit: u64,
    reservoir: u64,
) -> Option<PrecompileResult> {
    let input = ReadPrecompileInput { input: Bytes::copy_from_slice(data), gas_limit };
    let get = precompile_calls.get(&input)?;

    Some(match *get {
        ReadPrecompileResult::Ok { gas_used, ref bytes } => {
            Ok(PrecompileOutput::new(gas_used, bytes.clone(), reservoir))
        }
        ReadPrecompileResult::OutOfGas => {
            // Use all the gas passed to this precompile
            Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, reservoir))
        }
        ReadPrecompileResult::Error => {
            Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, reservoir))
        }
        ReadPrecompileResult::UnexpectedError => panic!("unexpected precompile error"),
    })
}

/// Answers a read precompile call that the block did not record.
///
/// Without a forwarder there is nothing to answer with, so the call burns its gas limit — which
/// is the historical behaviour for any unrecorded input.
fn run_unrecorded_precompile(
    forwarder: Option<&ScopedReadPrecompileForwarder>,
    address: Address,
    data: &[u8],
    gas_limit: u64,
    reservoir: u64,
    block_number: u64,
) -> PrecompileResult {
    match forwarder {
        Some((forwarder, head_hash)) => {
            forwarder.call(address, data, gas_limit, reservoir, block_number, *head_hash)
        }
        None => Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, reservoir)),
    }
}

impl<'a, EVM, Spec, R: ReceiptBuilder> HlBlockExecutor<'a, EVM, Spec, R>
where
    EVM: Evm<
            DB: StateDB,
            Precompiles = PrecompilesMap,
            Tx: FromRecoveredTx<R::Transaction>
                    + FromRecoveredTx<TransactionSigned>
                    + FromTxWithEncoded<TransactionSigned>,
        >,
    Spec: EthereumHardforks + HlHardforks + EthChainSpec + Hardforks + Clone,
    R: ReceiptBuilder<Transaction = TransactionSigned, Receipt: TxReceipt>,
    <R as ReceiptBuilder>::Transaction: Unpin + From<TransactionSigned>,
    <EVM as alloy_evm::Evm>::Tx: FromTxWithEncoded<<R as ReceiptBuilder>::Transaction>,
    HlTxEnv<TxEnv>: IntoTxEnv<<EVM as alloy_evm::Evm>::Tx>,
    R::Transaction: Into<TransactionSigned>,
{
    /// Creates a new HlBlockExecutor.
    pub fn new(mut evm: EVM, ctx: HlBlockExecutionCtx<'a>, spec: Spec, receipt_builder: R) -> Self {
        apply_precompiles_with_forwarder(&mut evm, &ctx.extras, ctx.read_precompile_forwarder());
        Self {
            spec,
            evm,
            gas_used: 0,
            receipts: vec![],
            receipt_builder,
            ctx,
            system_tx_receipts: vec![],
        }
    }

    fn deploy_corewriter_contract(&mut self) -> Result<(), BlockExecutionError> {
        const COREWRITER_ENABLED_BLOCK_NUMBER: u64 = 7578300;
        const COREWRITER_CONTRACT_ADDRESS: Address =
            address!("0x3333333333333333333333333333333333333333");
        const COREWRITER_CODE: &[u8] = &hex!(
            "608060405234801561000f575f5ffd5b5060043610610029575f3560e01c806317938e131461002d575b5f5ffd5b61004760048036038101906100429190610123565b610049565b005b5f5f90505b61019081101561006557808060010191505061004e565b503373ffffffffffffffffffffffffffffffffffffffff167f8c7f585fb295f7eb1e6aeb8fba61b23a4fe60beda405f0045073b185c74412e383836040516100ae9291906101c8565b60405180910390a25050565b5f5ffd5b5f5ffd5b5f5ffd5b5f5ffd5b5f5ffd5b5f5f83601f8401126100e3576100e26100c2565b5b8235905067ffffffffffffffff811115610100576100ff6100c6565b5b60208301915083600182028301111561011c5761011b6100ca565b5b9250929050565b5f5f60208385031215610139576101386100ba565b5b5f83013567ffffffffffffffff811115610156576101556100be565b5b610162858286016100ce565b92509250509250929050565b5f82825260208201905092915050565b828183375f83830152505050565b5f601f19601f8301169050919050565b5f6101a7838561016e565b93506101b483858461017e565b6101bd8361018c565b840190509392505050565b5f6020820190508181035f8301526101e181848661019c565b9050939250505056fea2646970667358221220f01517e1fbaff8af4bd72cb063cccecbacbb00b07354eea7dd52265d355474fb64736f6c634300081c0033"
        );

        if self.evm.block().number() != U256::from(COREWRITER_ENABLED_BLOCK_NUMBER) {
            return Ok(());
        }

        let corewriter_code = Bytecode::new_raw(COREWRITER_CODE.into());
        let mut info = self
            .evm
            .db_mut()
            .basic(COREWRITER_CONTRACT_ADDRESS)
            .map_err(BlockExecutionError::other)?
            .unwrap_or_default();

        info.code_hash = corewriter_code.hash_slow();
        info.code = Some(corewriter_code);

        // The generic `StateDB` bound only exposes `Database`/`DatabaseCommit`, so the code is
        // installed by committing a touched account rather than through a state transition.
        let mut account = Account::from(info);
        account.mark_touch();
        self.evm.db_mut().commit(HashMap::from_iter([(COREWRITER_CONTRACT_ADDRESS, account)]));
        Ok(())
    }
}

impl<E, Spec, R> BlockExecutor for HlBlockExecutor<'_, E, Spec, R>
where
    E: Evm<
            DB: StateDB,
            Tx: FromRecoveredTx<R::Transaction>
                    + FromRecoveredTx<TransactionSigned>
                    + FromTxWithEncoded<TransactionSigned>,
            Precompiles = PrecompilesMap,
        >,
    Spec: EthereumHardforks + HlHardforks + EthChainSpec + Hardforks,
    R: ReceiptBuilder<Transaction = TransactionSigned, Receipt: TxReceipt>,
    <R as ReceiptBuilder>::Transaction: Unpin + From<TransactionSigned>,
    <E as alloy_evm::Evm>::Tx: FromTxWithEncoded<<R as ReceiptBuilder>::Transaction>,
    HlTxEnv<TxEnv>: IntoTxEnv<<E as alloy_evm::Evm>::Tx>,
    R::Transaction: Into<TransactionSigned>,
{
    type Transaction = TransactionSigned;
    type Receipt = R::Receipt;
    type Evm = E;
    type Result = HlTxResult<<E as Evm>::HaltReason>;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        apply_precompiles_with_forwarder(
            &mut self.evm,
            &self.ctx.extras,
            self.ctx.read_precompile_forwarder(),
        );
        self.deploy_corewriter_contract()?;

        Ok(())
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError> {
        let (tx_env, tx) = tx.into_parts();

        // The sum of the transaction's gas limit, Tg, and the gas utilized in this block prior,
        // must be no greater than the block's gasLimit.
        let block_available_gas = self.evm.block().gas_limit() - self.gas_used;

        if tx.tx().gas_limit() > block_available_gas {
            return Err(BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                transaction_gas_limit: tx.tx().gas_limit(),
                block_available_gas,
            }
            .into());
        }

        // HL: `commit_transaction` no longer receives the transaction, so everything the commit
        // step needs from it is captured here.
        let is_system = is_system_transaction(tx.tx());
        let tx_type = tx.tx().tx_type();

        // Execute transaction and return the result
        let result = self.evm.transact(tx_env).map_err(|err| {
            let hash = tx.tx().trie_hash();
            BlockExecutionError::evm(err, hash)
        })?;

        Ok(HlTxResult { result, tx_type, is_system })
    }

    fn commit_transaction(&mut self, output: Self::Result) -> GasOutput {
        let HlTxResult { result: ResultAndState { result, mut state }, tx_type, is_system } =
            output;

        let gas_used = result.gas().tx_gas_used();

        // append gas used
        if !is_system {
            self.gas_used += gas_used;
        }

        // apply patches after
        patch_mainnet_after_tx(
            self.evm.block().number().saturating_to(),
            self.receipts.len() as u64,
            is_system,
            &mut state,
        )
        .expect("failed to apply mainnet patch");

        // The commitment covers the system txs the block source put at the start of the block.
        if self.ctx.extras.system_tx_receipts_hash.is_some()
            && self.receipts.len() < self.ctx.system_tx_count
        {
            let cumulative_gas_used =
                self.system_tx_receipts.last().map_or(0, |(_, _, gas, _)| *gas) + gas_used;
            self.system_tx_receipts.push((
                tx_type.ty(),
                result.is_success(),
                cumulative_gas_used,
                result.logs().to_vec(),
            ));
        }

        // Push transaction changeset and calculate header bloom filter for receipt.
        self.receipts.push(self.receipt_builder.build_receipt(ReceiptBuilderCtx {
            tx_type,
            evm: &self.evm,
            result,
            state: &state,
            cumulative_gas_used: self.gas_used,
        }));

        // Commit the state changes.
        self.evm.db_mut().commit(state);

        GasOutput::new(gas_used)
    }

    fn finish(self) -> Result<(Self::Evm, BlockExecutionResult<R::Receipt>), BlockExecutionError> {
        // System tx receipts are not committed to by the header, so check them against the block
        // source's commitment when it supplied one.
        if let Some(expected) = self.ctx.extras.system_tx_receipts_hash {
            let got = system_tx_receipts_hash(
                self.system_tx_receipts.iter().map(|(t, s, g, l)| (*t, *s, *g, l.as_slice())),
            );
            if got != expected {
                warn!(
                    receipts = ?self.system_tx_receipts,
                    %expected,
                    %got,
                    "system tx receipts mismatch"
                );
                return Err(BlockValidationError::msg(format!(
                    "system tx receipts hash mismatch: got {got}, expected {expected}"
                ))
                .into());
            }
        }
        Ok((
            self.evm,
            BlockExecutionResult {
                receipts: self.receipts,
                requests: Requests::default(),
                gas_used: self.gas_used,
                blob_gas_used: 0,
            },
        ))
    }

    fn receipts(&self) -> &[Self::Receipt] {
        &self.receipts
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        &mut self.evm
    }

    fn evm(&self) -> &Self::Evm {
        &self.evm
    }
}

pub fn apply_precompiles<EVM>(evm: &mut EVM, extras: &HlExtras)
where
    EVM: Evm<Precompiles = PrecompilesMap>,
{
    apply_precompiles_with_forwarder(evm, extras, None)
}

/// Same as [`apply_precompiles`], but resolves read precompile inputs that the block did not
/// record through `forwarder` instead of burning their gas limit.
///
/// Only the RPC call paths pass a forwarder, and only for the chain head. Block execution must
/// never forward: the recorded calls are consensus data, and an unrecorded input there means the
/// block itself is wrong.
pub fn apply_precompiles_with_forwarder<EVM>(
    evm: &mut EVM,
    extras: &HlExtras,
    forwarder: Option<ScopedReadPrecompileForwarder>,
) where
    EVM: Evm<Precompiles = PrecompilesMap>,
{
    let block_number = evm.block().number();
    let precompiles_mut = evm.precompiles_mut();
    // For all precompile addresses just in case it's populated and not cleared
    // Clear 0x00...08xx addresses
    let addresses = precompiles_mut.addresses().cloned().collect::<Vec<_>>();
    for address in addresses {
        if address.starts_with(&[0u8; 18]) && address[18] == 8 {
            precompiles_mut.apply_precompile(&address, |_| None);
        }
    }
    for (address, precompile) in extras.read_precompile_calls.clone().unwrap_or_default().0.iter() {
        let precompile = precompile.clone();
        let forwarder = forwarder.clone();
        let address = *address;
        precompiles_mut.apply_precompile(&address, |_| {
            let precompiles_map: HashMap<ReadPrecompileInput, ReadPrecompileResult> =
                precompile.iter().map(|(input, result)| (input.clone(), result.clone())).collect();
            Some(DynPrecompile::from(move |input: PrecompileInput| -> PrecompileResult {
                run_precompile(&precompiles_map, input.data, input.gas, input.reservoir)
                    .unwrap_or_else(|| {
                        run_unrecorded_precompile(
                            forwarder.as_ref(),
                            address,
                            input.data,
                            input.gas,
                            input.reservoir,
                            block_number.saturating_to(),
                        )
                    })
            }))
        });
    }

    // NOTE: This is adapted from hyperliquid-dex/hyper-evm-sync#5
    const WARM_PRECOMPILES_BLOCK_NUMBER: u64 = 8_197_684;
    if block_number >= U256::from(WARM_PRECOMPILES_BLOCK_NUMBER) {
        fill_all_precompiles(extras, precompiles_mut, forwarder, block_number.saturating_to());
    }
}

fn address_to_u64(address: Address) -> u64 {
    address.into_u256().try_into().unwrap()
}

fn fill_all_precompiles(
    extras: &HlExtras,
    precompiles_mut: &mut PrecompilesMap,
    forwarder: Option<ScopedReadPrecompileForwarder>,
    block_number: u64,
) {
    let lowest_address = 0x800;
    let highest_address = extras.highest_precompile_address.map_or(0x80D, address_to_u64);
    for address in lowest_address..=highest_address {
        let address = Address::from(U160::from(address));
        let forwarder = forwarder.clone();
        precompiles_mut.apply_precompile(&address, |f| {
            if let Some(precompile) = f {
                return Some(precompile);
            }

            Some(DynPrecompile::from(move |input: PrecompileInput| -> PrecompileResult {
                run_unrecorded_precompile(
                    forwarder.as_ref(),
                    address,
                    input.data,
                    input.gas,
                    input.reservoir,
                    block_number,
                )
            }))
        });
    }
}
