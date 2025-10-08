use super::HlEvmInner;
use crate::evm::{spec::HlSpecId, transaction::HlTxTr};
use reth_revm::context::ContextTr;
use revm::{
    context::Cfg, context_interface::Block, handler::instructions::EthInstructions,
    interpreter::interpreter::EthInterpreter, Context, Database,
};

/// Trait that allows for hl HlEvm to be built.
pub trait HlBuilder: Sized {
    /// Type of the context.
    type Context: ContextTr;

    /// Build the hl with an inspector.
    fn build_hl_with_inspector<INSP>(
        self,
        inspector: INSP,
    ) -> HlEvmInner<Self::Context, INSP, EthInstructions<EthInterpreter, Self::Context>>;
}

impl<BLOCK, TX, CFG, DB> HlBuilder for Context<BLOCK, TX, CFG, DB>
where
    BLOCK: Block,
    TX: HlTxTr,
    CFG: Cfg<Spec = HlSpecId>,
    DB: Database,
{
    type Context = Self;

    fn build_hl_with_inspector<INSP>(
        self,
        inspector: INSP,
    ) -> HlEvmInner<Self::Context, INSP, EthInstructions<EthInterpreter, Self::Context>> {
        HlEvmInner::new(self, inspector)
    }
}
