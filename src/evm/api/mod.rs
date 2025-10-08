use revm::{
    bytecode::opcode::BLOCKHASH,
    context::{ContextSetters, Evm, FrameStack},
    context_interface::ContextTr,
    handler::{
        evm::{ContextDbError, FrameInitResult},
        instructions::{EthInstructions, InstructionProvider},
        EthFrame, EthPrecompiles, EvmTr, FrameInitOrResult, FrameTr, PrecompileProvider,
    },
    inspector::{InspectorEvmTr, JournalExt},
    interpreter::{interpreter::EthInterpreter, Instruction, InterpreterResult},
    Inspector,
};

use crate::chainspec::MAINNET_CHAIN_ID;

pub mod builder;
pub mod ctx;
mod exec;
mod patch;

pub struct HlEvmInner<
    CTX: ContextTr,
    INSP,
    I = EthInstructions<EthInterpreter, CTX>,
    P = EthPrecompiles,
>(pub Evm<CTX, INSP, I, P, EthFrame<EthInterpreter>>);

impl<CTX: ContextTr, INSP>
    HlEvmInner<CTX, INSP, EthInstructions<EthInterpreter, CTX>, EthPrecompiles>
{
    pub fn new(ctx: CTX, inspector: INSP) -> Self {
        let mut instruction = EthInstructions::new_mainnet();

        const NON_PLACEHOLDER_BLOCK_HASH_HEIGHT: u64 = 243_538;
        if ctx.chain_id() == MAINNET_CHAIN_ID &&
            ctx.block_number() < NON_PLACEHOLDER_BLOCK_HASH_HEIGHT
        {
            instruction.insert_instruction(
                BLOCKHASH,
                Instruction::new(patch::blockhash_returning_placeholder, 20),
            );
        }

        Self(Evm {
            ctx,
            inspector,
            instruction,
            precompiles: EthPrecompiles::default(),
            frame_stack: FrameStack::new(),
        })
    }

    /// Consumes self and returns a new Evm type with given Precompiles.
    pub fn with_precompiles<OP>(
        self,
        precompiles: OP,
    ) -> HlEvmInner<CTX, INSP, EthInstructions<EthInterpreter, CTX>, OP> {
        HlEvmInner(self.0.with_precompiles(precompiles))
    }
}

impl<CTX, INSP, I, P> InspectorEvmTr for HlEvmInner<CTX, INSP, I, P>
where
    CTX: ContextTr<Journal: JournalExt> + ContextSetters,
    I: InstructionProvider<Context = CTX, InterpreterTypes = EthInterpreter>,
    INSP: Inspector<CTX, I::InterpreterTypes>,
    P: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    type Inspector = INSP;

    fn inspector(&mut self) -> &mut Self::Inspector {
        &mut self.0.inspector
    }

    fn ctx_inspector(&mut self) -> (&mut Self::Context, &mut Self::Inspector) {
        (&mut self.0.ctx, &mut self.0.inspector)
    }

    fn ctx_inspector_frame(
        &mut self,
    ) -> (&mut Self::Context, &mut Self::Inspector, &mut Self::Frame) {
        (&mut self.0.ctx, &mut self.0.inspector, self.0.frame_stack.get())
    }

    fn ctx_inspector_frame_instructions(
        &mut self,
    ) -> (&mut Self::Context, &mut Self::Inspector, &mut Self::Frame, &mut Self::Instructions) {
        (&mut self.0.ctx, &mut self.0.inspector, self.0.frame_stack.get(), &mut self.0.instruction)
    }
}

impl<CTX, INSP, I, P> EvmTr for HlEvmInner<CTX, INSP, I, P>
where
    CTX: ContextTr,
    I: InstructionProvider<Context = CTX, InterpreterTypes = EthInterpreter>,
    P: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    type Context = CTX;
    type Instructions = I;
    type Precompiles = P;
    type Frame = EthFrame<EthInterpreter>;

    fn ctx(&mut self) -> &mut Self::Context {
        &mut self.0.ctx
    }

    fn ctx_ref(&self) -> &Self::Context {
        &self.0.ctx
    }

    fn ctx_instructions(&mut self) -> (&mut Self::Context, &mut Self::Instructions) {
        (&mut self.0.ctx, &mut self.0.instruction)
    }

    fn ctx_precompiles(&mut self) -> (&mut Self::Context, &mut Self::Precompiles) {
        (&mut self.0.ctx, &mut self.0.precompiles)
    }

    fn frame_stack(&mut self) -> &mut FrameStack<Self::Frame> {
        &mut self.0.frame_stack
    }

    fn frame_init(
        &mut self,
        frame_input: <Self::Frame as FrameTr>::FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        self.0.frame_init(frame_input)
    }

    fn frame_run(
        &mut self,
    ) -> Result<FrameInitOrResult<Self::Frame>, ContextDbError<Self::Context>> {
        self.0.frame_run()
    }

    fn frame_return_result(
        &mut self,
        result: <Self::Frame as FrameTr>::FrameResult,
    ) -> Result<Option<<Self::Frame as FrameTr>::FrameResult>, ContextDbError<Self::Context>> {
        self.0.frame_return_result(result)
    }
}
