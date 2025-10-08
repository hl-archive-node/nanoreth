use super::HlEvm;
use crate::evm::{
    api::{
        builder::HlBuilder,
        ctx::{DefaultHl, HlContext},
    },
    spec::HlSpecId,
    transaction::HlTxEnv,
};
use reth_evm::{precompiles::PrecompilesMap, Database, EvmEnv, EvmFactory};
use reth_revm::Context;
use revm::{
    context::{
        result::{EVMError, HaltReason},
        TxEnv,
    },
    inspector::NoOpInspector,
    precompile::{PrecompileSpecId, Precompiles},
    Inspector,
};

/// Factory producing [`HlEvm`].
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct HlEvmFactory;

impl EvmFactory for HlEvmFactory {
    type Evm<DB: Database, I: Inspector<HlContext<DB>>> = HlEvm<DB, I, Self::Precompiles>;
    type Context<DB: Database> = HlContext<DB>;
    type Tx = HlTxEnv<TxEnv>;
    type Error<DBError: core::error::Error + Send + Sync + 'static> = EVMError<DBError>;
    type HaltReason = HaltReason;
    type Spec = HlSpecId;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<HlSpecId>,
    ) -> Self::Evm<DB, NoOpInspector> {
        let spec_id = *input.spec_id();
        HlEvm {
            inner: Context::hl()
                .with_block(input.block_env)
                .with_cfg(input.cfg_env)
                .with_db(db)
                .build_hl_with_inspector(NoOpInspector {})
                .with_precompiles(hl_precompiles(spec_id)),
            inspect: false,
        }
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
        HlEvm {
            inner: Context::hl()
                .with_block(input.block_env)
                .with_cfg(input.cfg_env)
                .with_db(db)
                .build_hl_with_inspector(inspector)
                .with_precompiles(hl_precompiles(spec_id)),
            inspect: true,
        }
    }
}

fn hl_precompiles(spec_id: HlSpecId) -> PrecompilesMap {
    let spec = PrecompileSpecId::from_spec_id(spec_id.into());
    PrecompilesMap::from_static(Precompiles::new(spec))
}
