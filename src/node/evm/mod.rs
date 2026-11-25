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
pub mod factory;
pub mod mrb;
mod patch;
pub mod read_precompile_forwarder;
pub mod receipt_builder;

pub use executor::{
    ScopedReadPrecompileForwarder, apply_precompiles, apply_precompiles_with_forwarder,
};

/// The revm EVM underlying [`HlEvm`].
pub type HlEvmCore<DB, I, P> =
    HlEvmInner<HlContext<DB>, I, EthInstructions<EthInterpreter, HlContext<DB>>, P>;

/// HL EVM implementation.
///
/// This is a wrapper type around the `revm` evm with optional [`Inspector`] (tracing)
/// support. [`Inspector`] support is configurable at runtime because it's part of the underlying
/// [`HlEvmCore`] type.
#[allow(missing_debug_implementations)]
pub struct HlEvm<DB: Database, I, P = EthPrecompiles> {
    pub inner: HlEvmCore<DB, I, P>,
    pub inspect: bool,
}

impl<DB: Database, I, P> HlEvm<DB, I, P> {
    /// The core EVM.
    #[inline]
    pub fn core(&self) -> &HlEvmCore<DB, I, P> {
        &self.inner
    }

    /// Mutable counterpart of [`Self::core`].
    #[inline]
    pub fn core_mut(&mut self) -> &mut HlEvmCore<DB, I, P> {
        &mut self.inner
    }

    /// Consumes the wrapper and returns the core EVM.
    #[inline]
    pub fn into_core(self) -> HlEvmCore<DB, I, P> {
        self.inner
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
        // hl-node, like geth with `NoBaseFee`, executes zero gas price transactions (system txs)
        // with a block base fee of zero, which is what the BASEFEE opcode observes.
        let zero_fee = tx.base.gas_price == 0;
        let basefee = self.ctx().block.basefee;
        if zero_fee {
            self.ctx_mut().block.basefee = 0;
        }
        let result = if self.inspect { self.inner.inspect_tx(tx) } else { self.inner.transact(tx) };
        if zero_fee {
            self.ctx_mut().block.basefee = basefee;
        }
        result
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
        let Context { block: block_env, cfg: cfg_env, journaled_state, .. } =
            self.into_core().0.ctx;

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
        (&mut core.0.ctx.journaled_state.database, &mut core.0.inspector, &mut core.0.precompiles)
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
