use crate::evm::{spec::HlSpecId, transaction::HlTxEnv};
use revm::{
    context::{BlockEnv, CfgEnv, TxEnv},
    database_interface::EmptyDB,
    Context, Journal, MainContext,
};

/// Type alias for the default context type of the HlEvm.
pub type HlContext<DB> = Context<BlockEnv, HlTxEnv<TxEnv>, CfgEnv<HlSpecId>, DB, Journal<DB>>;

/// Trait that allows for a default context to be created.
pub trait DefaultHl {
    /// Create a default context.
    fn hl() -> HlContext<EmptyDB>;
}

impl DefaultHl for HlContext<EmptyDB> {
    fn hl() -> Self {
        Context::mainnet()
            .with_tx(HlTxEnv::default())
            .with_cfg(CfgEnv::new_with_spec(HlSpecId::default()))
    }
}
