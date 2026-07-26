use crate::{
    evm::{
        api::{HlEvmInner, ctx::HlContext},
        spec::HlSpecId,
        transaction::HlTxEnv,
    },
    node::HlNode,
};
use alloy_primitives::{Address, Bytes};
use config::HlEvmConfig;
use reth::{
    api::FullNodeTypes,
    builder::{BuilderContext, components::ExecutorBuilder},
};
use reth_evm::{Database, Evm, EvmEnv};
use revm::{
    Context, ExecuteEvm, InspectEvm, Inspector,
    context::{
        BlockEnv, CfgEnv, TxEnv,
        result::{
            EVMError, ExecutionResult, HaltReason, Output, ResultAndState, ResultGas, SuccessReason,
        },
    },
    handler::{EthPrecompiles, PrecompileProvider, instructions::EthInstructions},
    interpreter::{InterpreterResult, interpreter::EthInterpreter},
    state::EvmState,
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
/// The revm EVM underlying [`HlEvm`], before any JIT layer.
pub type HlEvmCore<DB, I, P> =
    HlEvmInner<HlContext<DB>, I, EthInstructions<EthInterpreter, HlContext<DB>>, P>;

/// With the `jit` feature the core EVM is wrapped by revmc, which overrides frame execution to
/// dispatch to compiled code when the contract is hot and falls back to the interpreter otherwise.
#[cfg(feature = "jit")]
pub type HlEvmExecutor<DB, I, P> = reth_evm_ethereum::factory::JitEvm<HlEvmCore<DB, I, P>>;
#[cfg(not(feature = "jit"))]
pub type HlEvmExecutor<DB, I, P> = HlEvmCore<DB, I, P>;

#[allow(missing_debug_implementations)]
pub struct HlEvm<DB: Database, I, P = EthPrecompiles> {
    pub inner: HlEvmExecutor<DB, I, P>,
    pub inspect: bool,
}

impl<DB: Database, I, P> HlEvm<DB, I, P> {
    /// The core EVM, reached through the JIT wrapper when it is present.
    #[inline]
    pub fn core(&self) -> &HlEvmCore<DB, I, P> {
        #[cfg(feature = "jit")]
        {
            self.inner.inner()
        }
        #[cfg(not(feature = "jit"))]
        {
            &self.inner
        }
    }

    /// Mutable counterpart of [`Self::core`].
    #[inline]
    pub fn core_mut(&mut self) -> &mut HlEvmCore<DB, I, P> {
        #[cfg(feature = "jit")]
        {
            self.inner.inner_mut()
        }
        #[cfg(not(feature = "jit"))]
        {
            &mut self.inner
        }
    }

    /// Consumes the wrapper and returns the core EVM.
    #[inline]
    pub fn into_core(self) -> HlEvmCore<DB, I, P> {
        #[cfg(feature = "jit")]
        {
            self.inner.into_inner()
        }
        #[cfg(not(feature = "jit"))]
        {
            self.inner
        }
    }

    /// Provides a reference to the EVM context.
    pub fn ctx(&self) -> &HlContext<DB> {
        &self.core().0.ctx
    }

    /// Provides a mutable reference to the EVM context.
    pub fn ctx_mut(&mut self) -> &mut HlContext<DB> {
        &mut self.core_mut().0.ctx
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
    type BlockEnv = BlockEnv;
    type Precompiles = P;
    type Inspector = I;

    fn chain_id(&self) -> u64 {
        self.cfg.chain_id
    }

    fn block(&self) -> &BlockEnv {
        &self.block
    }

    fn cfg_env(&self) -> &CfgEnv<Self::Spec> {
        &self.cfg
    }

    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        if self.inspect { self.inner.inspect_tx(tx) } else { self.inner.transact(tx) }
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
                gas: ResultGas::default(),
                logs: vec![],
                output: Output::Call(Bytes::new()),
            },
            EvmState::default(),
        ))
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec>) {
        let Context { block: block_env, cfg: cfg_env, journaled_state, .. } = self.into_core().0.ctx;

        (journaled_state.database, EvmEnv { block_env, cfg_env })
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inspect = enabled;
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        let core = self.core();
        (&core.0.ctx.journaled_state.database, &core.0.inspector, &core.0.precompiles)
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        let core = self.core_mut();
        (
            &mut core.0.ctx.journaled_state.database,
            &mut core.0.inspector,
            &mut core.0.precompiles,
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
