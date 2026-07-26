use crate::{HlBlock, HlPrimitives};
use alloy_eips::eip7685::Requests;
use alloy_primitives::U256;
use reth_payload_primitives::BuiltPayload;
use reth_primitives_traits::SealedBlock;
use std::sync::Arc;

/// Built payload for Hl. This is similar to [`EthBuiltPayload`] but without sidecars as those
/// included into [`HlBlock`].
#[derive(Debug, Clone)]
pub struct HlBuiltPayload {
    /// The built block
    pub(crate) block: Arc<SealedBlock<HlBlock>>,
    /// The fees of the block
    pub(crate) fees: U256,
    /// The requests of the payload
    pub(crate) requests: Option<Requests>,
}

impl BuiltPayload for HlBuiltPayload {
    type Primitives = HlPrimitives;

    fn block(&self) -> &SealedBlock<HlBlock> {
        self.block.as_ref()
    }

    fn fees(&self) -> U256 {
        self.fees
    }

    fn requests(&self) -> Option<Requests> {
        self.requests.clone()
    }
}
