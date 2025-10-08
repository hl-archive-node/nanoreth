use super::{config::HlBlockExecutionCtx, patch::patch_mainnet_after_tx};
use crate::{
    evm::transaction::HlTxEnv,
    hardforks::HlHardforks,
    node::{
        primitives::TransactionSigned,
        types::{HlExtras, ReadPrecompileInput, ReadPrecompileResult},
    },
};
use alloy_consensus::{Transaction, TxReceipt};
use alloy_eips::{eip7685::Requests, Encodable2718};
use alloy_evm::{block::ExecutableTx, eth::receipt_builder::ReceiptBuilderCtx};
use alloy_primitives::{address, hex, Address, Bytes, U160, U256};
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use reth_evm::{
    block::{BlockValidationError, CommitChanges},
    eth::receipt_builder::ReceiptBuilder,
    execute::{BlockExecutionError, BlockExecutor},
    precompiles::{DynPrecompile, PrecompileInput, PrecompilesMap},
    Database, Evm, FromRecoveredTx, FromTxWithEncoded, IntoTxEnv, OnStateHook,
};
use reth_provider::BlockExecutionResult;
use reth_revm::State;
use revm::{
    context::{
        result::{ExecutionResult, ResultAndState},
        TxEnv,
    },
    interpreter::instructions::utility::IntoU256,
    precompile::{PrecompileError, PrecompileOutput, PrecompileResult},
    primitives::HashMap,
    state::Bytecode,
    DatabaseCommit,
};

pub fn is_system_transaction(tx: &TransactionSigned) -> bool {
    let Some(gas_price) = tx.gas_price() else {
        return false;
    };
    gas_price == 0
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
}

fn run_precompile(
    precompile_calls: &HashMap<ReadPrecompileInput, ReadPrecompileResult>,
    data: &[u8],
    gas_limit: u64,
) -> PrecompileResult {
    let input = ReadPrecompileInput { input: Bytes::copy_from_slice(data), gas_limit };
    let Some(get) = precompile_calls.get(&input) else {
        return Err(PrecompileError::OutOfGas);
    };

    match *get {
        ReadPrecompileResult::Ok { gas_used, ref bytes } => {
            Ok(PrecompileOutput { gas_used, bytes: bytes.clone(), reverted: false })
        }
        ReadPrecompileResult::OutOfGas => {
            // Use all the gas passed to this precompile
            Err(PrecompileError::OutOfGas)
        }
        ReadPrecompileResult::Error => Err(PrecompileError::OutOfGas),
        ReadPrecompileResult::UnexpectedError => panic!("unexpected precompile error"),
    }
}

impl<'a, DB, EVM, Spec, R: ReceiptBuilder> HlBlockExecutor<'a, EVM, Spec, R>
where
    DB: Database + 'a,
    EVM: Evm<
        DB = &'a mut State<DB>,
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
        apply_precompiles(&mut evm, &ctx.extras);
        Self { spec, evm, gas_used: 0, receipts: vec![], receipt_builder, ctx }
    }

    fn deploy_corewriter_contract(&mut self) -> Result<(), BlockExecutionError> {
        const COREWRITER_ENABLED_BLOCK_NUMBER: u64 = 7578300;
        const COREWRITER_CONTRACT_ADDRESS: Address =
            address!("0x3333333333333333333333333333333333333333");
        const COREWRITER_CODE: &[u8] = &hex!("608060405234801561000f575f5ffd5b5060043610610029575f3560e01c806317938e131461002d575b5f5ffd5b61004760048036038101906100429190610123565b610049565b005b5f5f90505b61019081101561006557808060010191505061004e565b503373ffffffffffffffffffffffffffffffffffffffff167f8c7f585fb295f7eb1e6aeb8fba61b23a4fe60beda405f0045073b185c74412e383836040516100ae9291906101c8565b60405180910390a25050565b5f5ffd5b5f5ffd5b5f5ffd5b5f5ffd5b5f5ffd5b5f5f83601f8401126100e3576100e26100c2565b5b8235905067ffffffffffffffff811115610100576100ff6100c6565b5b60208301915083600182028301111561011c5761011b6100ca565b5b9250929050565b5f5f60208385031215610139576101386100ba565b5b5f83013567ffffffffffffffff811115610156576101556100be565b5b610162858286016100ce565b92509250509250929050565b5f82825260208201905092915050565b828183375f83830152505050565b5f601f19601f8301169050919050565b5f6101a7838561016e565b93506101b483858461017e565b6101bd8361018c565b840190509392505050565b5f6020820190508181035f8301526101e181848661019c565b9050939250505056fea2646970667358221220f01517e1fbaff8af4bd72cb063cccecbacbb00b07354eea7dd52265d355474fb64736f6c634300081c0033");

        if self.evm.block().number != U256::from(COREWRITER_ENABLED_BLOCK_NUMBER) {
            return Ok(());
        }

        let corewriter_code = Bytecode::new_raw(COREWRITER_CODE.into());
        let account = self
            .evm
            .db_mut()
            .load_cache_account(COREWRITER_CONTRACT_ADDRESS)
            .map_err(BlockExecutionError::other)?;

        let mut info = account.account_info().unwrap_or_default();
        info.code_hash = corewriter_code.hash_slow();
        info.code = Some(corewriter_code);

        let transition = account.change(info, Default::default());
        self.evm.db_mut().apply_transition(vec![(COREWRITER_CONTRACT_ADDRESS, transition)]);
        Ok(())
    }
}

impl<'a, DB, E, Spec, R> BlockExecutor for HlBlockExecutor<'a, E, Spec, R>
where
    DB: Database + 'a,
    E: Evm<
        DB = &'a mut State<DB>,
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

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        apply_precompiles(&mut self.evm, &self.ctx.extras);
        self.deploy_corewriter_contract()?;

        Ok(())
    }

    fn execute_transaction_with_commit_condition(
        &mut self,
        tx: impl ExecutableTx<Self>,
        f: impl FnOnce(&ExecutionResult<<Self::Evm as Evm>::HaltReason>) -> CommitChanges,
    ) -> Result<Option<u64>, BlockExecutionError> {
        // The sum of the transaction's gas limit, Tg, and the gas utilized in this block prior,
        // must be no greater than the block's gasLimit.
        let block_available_gas = self.evm.block().gas_limit - self.gas_used;

        if tx.tx().gas_limit() > block_available_gas {
            return Err(BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                transaction_gas_limit: tx.tx().gas_limit(),
                block_available_gas,
            }
            .into());
        }

        // Execute transaction.
        let ResultAndState { result, mut state } = self
            .evm
            .transact(&tx)
            .map_err(|err| BlockExecutionError::evm(err, tx.tx().trie_hash()))?;

        if !f(&result).should_commit() {
            return Ok(None);
        }

        let gas_used = result.gas_used();

        // append gas used
        if !is_system_transaction(tx.tx()) {
            self.gas_used += gas_used;
        }

        // apply patches after
        patch_mainnet_after_tx(
            self.evm.block().number.saturating_to(),
            self.receipts.len() as u64,
            is_system_transaction(tx.tx()),
            &mut state,
        )?;

        // Push transaction changeset and calculate header bloom filter for receipt.
        self.receipts.push(self.receipt_builder.build_receipt(ReceiptBuilderCtx {
            tx: tx.tx(),
            evm: &self.evm,
            result,
            state: &state,
            cumulative_gas_used: self.gas_used,
        }));

        // Commit the state changes.
        self.evm.db_mut().commit(state);

        Ok(Some(gas_used))
    }

    fn finish(self) -> Result<(Self::Evm, BlockExecutionResult<R::Receipt>), BlockExecutionError> {
        Ok((
            self.evm,
            BlockExecutionResult {
                receipts: self.receipts,
                requests: Requests::default(),
                gas_used: self.gas_used,
            },
        ))
    }

    fn set_state_hook(&mut self, _hook: Option<Box<dyn OnStateHook>>) {}

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
    let block_number = evm.block().number;
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
        precompiles_mut.apply_precompile(address, |_| {
            let precompiles_map: HashMap<ReadPrecompileInput, ReadPrecompileResult> =
                precompile.iter().map(|(input, result)| (input.clone(), result.clone())).collect();
            Some(DynPrecompile::from(move |input: PrecompileInput| -> PrecompileResult {
                run_precompile(&precompiles_map, input.data, input.gas)
            }))
        });
    }

    // NOTE: This is adapted from hyperliquid-dex/hyper-evm-sync#5
    const WARM_PRECOMPILES_BLOCK_NUMBER: u64 = 8_197_684;
    if block_number >= U256::from(WARM_PRECOMPILES_BLOCK_NUMBER) {
        fill_all_precompiles(extras, precompiles_mut);
    }
}

fn address_to_u64(address: Address) -> u64 {
    address.into_u256().try_into().unwrap()
}

fn fill_all_precompiles(extras: &HlExtras, precompiles_mut: &mut PrecompilesMap) {
    let lowest_address = 0x800;
    let highest_address = extras.highest_precompile_address.map_or(0x80D, address_to_u64);
    for address in lowest_address..=highest_address {
        let address = Address::from(U160::from(address));
        precompiles_mut.apply_precompile(&address, |f| {
            if let Some(precompile) = f {
                return Some(precompile);
            }

            Some(DynPrecompile::from(move |_: PrecompileInput| -> PrecompileResult {
                Err(PrecompileError::OutOfGas)
            }))
        });
    }
}
