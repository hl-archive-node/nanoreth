use super::{HlEvm, HlEvmCore, HlEvmExecutor};
use crate::evm::{
    api::{
        builder::HlBuilder,
        ctx::{DefaultHl, HlContext},
    },
    spec::HlSpecId,
    transaction::HlTxEnv,
};
use reth_evm::{Database, EvmEnv, EvmFactory, precompiles::PrecompilesMap};
use reth_revm::Context;
use revm::{
    Inspector,
    context::{
        BlockEnv, TxEnv,
        result::{EVMError, HaltReason},
    },
    database_interface::DBErrorMarker,
    inspector::NoOpInspector,
    precompile::{PrecompileSpecId, Precompiles},
};

/// Factory producing [`HlEvm`].
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct HlEvmFactory;

impl EvmFactory for HlEvmFactory {
    type Evm<DB: Database, I: Inspector<HlContext<DB>>> = HlEvm<DB, I, Self::Precompiles>;
    type Context<DB: Database> = HlContext<DB>;
    type Tx = HlTxEnv<TxEnv>;
    type Error<DBError: DBErrorMarker> = EVMError<DBError>;
    type HaltReason = HaltReason;
    type Spec = HlSpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<HlSpecId>,
    ) -> Self::Evm<DB, NoOpInspector> {
        let spec_id = *input.spec_id();
        let jit = jit_backend(&input.block_env, input.cfg_env.chain_id);
        let core = Context::hl()
            .with_block(input.block_env)
            .with_cfg(input.cfg_env)
            .with_db(db)
            .build_hl_with_inspector(NoOpInspector {})
            .with_precompiles(hl_precompiles(spec_id));
        HlEvm { inner: with_jit(core, jit), inspect: false }
    }

    fn create_evm_with_inspector<
        DB: Database<Error: Send + Sync + 'static>,
        I: Inspector<Self::Context<DB>>,
    >(
        &self,
        db: DB,
        input: EvmEnv<HlSpecId>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let spec_id = *input.spec_id();
        let jit = jit_backend(&input.block_env, input.cfg_env.chain_id);
        let core = Context::hl()
            .with_block(input.block_env)
            .with_cfg(input.cfg_env)
            .with_db(db)
            .build_hl_with_inspector(inspector)
            .with_precompiles(hl_precompiles(spec_id));
        HlEvm { inner: with_jit(core, jit), inspect: true }
    }
}

/// Resolves the JIT backend for a block, disabled where HL patches the instruction table.
#[cfg(feature = "jit")]
fn jit_backend(block_env: &BlockEnv, chain_id: u64) -> crate::evm::jit::JitBackend {
    use revm::context::Block;
    crate::evm::jit::backend_for_block(chain_id, block_env.number().saturating_to())
}

#[cfg(not(feature = "jit"))]
fn jit_backend(_block_env: &BlockEnv, _chain_id: u64) {}

/// Installs the JIT layer over the core EVM. Without the feature this is the identity.
#[cfg(feature = "jit")]
fn with_jit<DB: Database, I, P>(
    core: HlEvmCore<DB, I, P>,
    backend: crate::evm::jit::JitBackend,
) -> HlEvmExecutor<DB, I, P>
where
    P: revm::handler::PrecompileProvider<
            crate::evm::api::ctx::HlContext<DB>,
            Output = revm::interpreter::InterpreterResult,
        >,
{
    reth_evm_ethereum::factory::JitEvm::new(core, backend)
}

#[cfg(not(feature = "jit"))]
fn with_jit<DB: Database, I, P>(
    core: HlEvmCore<DB, I, P>,
    _backend: (),
) -> HlEvmExecutor<DB, I, P> {
    core
}

fn hl_precompiles(spec_id: HlSpecId) -> PrecompilesMap {
    let spec = PrecompileSpecId::from_spec_id(spec_id.into());
    PrecompilesMap::from_static(Precompiles::new(spec))
}
