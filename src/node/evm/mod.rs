use crate::{
    evm::{
        api::{ctx::HlContext, HlEvmInner},
        spec::HlSpecId,
        transaction::HlTxEnv,
    },
    node::HlNode,
};
use alloy_primitives::{Address, Bytes};
use config::HlEvmConfig;
use reth::{
    api::FullNodeTypes,
    builder::{components::ExecutorBuilder, BuilderContext},
};
use reth_evm::{Database, Evm, EvmEnv};
use revm::{
    context::{
        result::{EVMError, ExecutionResult, HaltReason, Output, ResultAndState, SuccessReason},
        BlockEnv, TxEnv,
    },
    handler::{instructions::EthInstructions, EthPrecompiles, PrecompileProvider},
    interpreter::{interpreter::EthInterpreter, InterpreterResult},
    state::EvmState,
    Context, ExecuteEvm, InspectEvm, Inspector,
};
use std::ops::{Deref, DerefMut};

mod assembler;
pub mod config;
mod executor;
mod factory;
mod patch;
pub mod receipt_builder;

pub use executor::apply_precompiles;

/// HL EVM implementation.
///
/// This is a wrapper type around the `revm` evm with optional [`Inspector`] (tracing)
/// support. [`Inspector`] support is configurable at runtime because it's part of the underlying
#[allow(missing_debug_implementations)]
pub struct HlEvm<DB: Database, I, P = EthPrecompiles> {
    pub inner: HlEvmInner<HlContext<DB>, I, EthInstructions<EthInterpreter, HlContext<DB>>, P>,
    pub inspect: bool,
}

impl<DB: Database, I, P> HlEvm<DB, I, P> {
    /// Provides a reference to the EVM context.
    pub const fn ctx(&self) -> &HlContext<DB> {
        &self.inner.0.ctx
    }

    /// Provides a mutable reference to the EVM context.
    pub fn ctx_mut(&mut self) -> &mut HlContext<DB> {
        &mut self.inner.0.ctx
    }
}

impl<DB: Database, I, P> Deref for HlEvm<DB, I, P> {
    type Target = HlContext<DB>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database, I, P> DerefMut for HlEvm<DB, I, P> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB, I, P> Evm for HlEvm<DB, I, P>
where
    DB: Database,
    I: Inspector<HlContext<DB>>,
    P: PrecompileProvider<HlContext<DB>, Output = InterpreterResult>,
{
    type DB = DB;
    type Tx = HlTxEnv<TxEnv>;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
    type Spec = HlSpecId;
    type Precompiles = P;
    type Inspector = I;

    fn chain_id(&self) -> u64 {
        self.cfg.chain_id
    }

    fn block(&self) -> &BlockEnv {
        &self.block
    }

    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        if self.inspect {
            self.inner.inspect_tx(tx)
        } else {
            self.inner.transact(tx)
        }
    }

    fn transact_system_call(
        &mut self,
        _caller: Address,
        _contract: Address,
        _data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        // NOTE: This is used for block traces.
        // Per hyper-evm-sync, HyperEVM doesn't seem to call this method, so
        // - we just return a success result with no changes, which gives the same semantics as
        //   HyperEVM.
        // - In a long term (ideally), consider implementing SystemCaller.
        Ok(ResultAndState::new(
            ExecutionResult::Success {
                reason: SuccessReason::Stop,
                gas_used: 0,
                gas_refunded: 0,
                logs: vec![],
                output: Output::Call(Bytes::new()),
            },
            EvmState::default(),
        ))
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec>) {
        let Context { block: block_env, cfg: cfg_env, journaled_state, .. } = self.inner.0.ctx;

        (journaled_state.database, EvmEnv { block_env, cfg_env })
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inspect = enabled;
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        (
            &self.inner.0.ctx.journaled_state.database,
            &self.inner.0.inspector,
            &self.inner.0.precompiles,
        )
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        (
            &mut self.inner.0.ctx.journaled_state.database,
            &mut self.inner.0.inspector,
            &mut self.inner.0.precompiles,
        )
    }
}

/// A regular hl evm and executor builder.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct HlExecutorBuilder;

impl<Node> ExecutorBuilder<Node> for HlExecutorBuilder
where
    Node: FullNodeTypes<Types = HlNode>,
{
    type EVM = HlEvmConfig;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        Ok(HlEvmConfig::hl(ctx.chain_spec()))
    }
}
