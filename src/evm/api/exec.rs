use super::HlEvmInner;
use crate::evm::{spec::HlSpecId, transaction::HlTxTr};
use revm::{
    context::{result::HaltReason, ContextSetters},
    context_interface::{
        result::{EVMError, ExecutionResult, ResultAndState},
        Cfg, ContextTr, Database, JournalTr,
    },
    handler::{instructions::EthInstructions, PrecompileProvider},
    inspector::{InspectCommitEvm, InspectEvm, Inspector, JournalExt},
    interpreter::{interpreter::EthInterpreter, InterpreterResult},
    state::EvmState,
    DatabaseCommit, ExecuteCommitEvm, ExecuteEvm,
};

// Type alias for HL context
pub trait HlContextTr:
    ContextTr<Journal: JournalTr<State = EvmState>, Tx: HlTxTr, Cfg: Cfg<Spec = HlSpecId>>
{
}

impl<T> HlContextTr for T where
    T: ContextTr<Journal: JournalTr<State = EvmState>, Tx: HlTxTr, Cfg: Cfg<Spec = HlSpecId>>
{
}

/// Type alias for the error type of the HlEvm.
type HlError<CTX> = EVMError<<<CTX as ContextTr>::Db as Database>::Error>;

impl<CTX, INSP, PRECOMPILE> ExecuteEvm
    for HlEvmInner<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: HlContextTr + ContextSetters,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    type ExecutionResult = ExecutionResult<HaltReason>;
    type State = EvmState;
    type Error = HlError<CTX>;

    type Tx = <CTX as ContextTr>::Tx;

    type Block = <CTX as ContextTr>::Block;

    #[inline]
    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.0.transact_one(tx)
    }

    #[inline]
    fn finalize(&mut self) -> Self::State {
        self.0.finalize()
    }

    #[inline]
    fn set_block(&mut self, block: Self::Block) {
        self.0.set_block(block);
    }

    #[inline]
    fn replay(&mut self) -> Result<ResultAndState<HaltReason>, Self::Error> {
        self.0.replay()
    }
}

impl<CTX, INSP, PRECOMPILE> ExecuteCommitEvm
    for HlEvmInner<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: HlContextTr<Db: DatabaseCommit> + ContextSetters,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    fn commit(&mut self, state: Self::State) {
        self.0.commit(state);
    }
}

impl<CTX, INSP, PRECOMPILE> InspectEvm
    for HlEvmInner<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: HlContextTr<Journal: JournalExt> + ContextSetters,
    INSP: Inspector<CTX, EthInterpreter>,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.0.set_inspector(inspector);
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.0.inspect_one_tx(tx)
    }
}

impl<CTX, INSP, PRECOMPILE> InspectCommitEvm
    for HlEvmInner<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: HlContextTr<Journal: JournalExt, Db: DatabaseCommit> + ContextSetters,
    INSP: Inspector<CTX, EthInterpreter>,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
}
