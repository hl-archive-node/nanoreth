use crate::node::{engine::HlBuiltPayload, rpc::engine_api::validator::HlExecutionData};
use alloy_primitives::Bytes;
use reth::primitives::{NodePrimitives, SealedBlock};
use reth_node_ethereum::engine::EthPayloadAttributes;
use reth_payload_primitives::{BuiltPayload, PayloadTypes};

/// A default payload type for [`HlPayloadTypes`]
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
#[non_exhaustive]
pub struct HlPayloadTypes;

impl PayloadTypes for HlPayloadTypes {
    type BuiltPayload = HlBuiltPayload;
    type PayloadAttributes = EthPayloadAttributes;
    type ExecutionData = HlExecutionData;

    fn block_to_payload(
        block: SealedBlock<
            <<Self::BuiltPayload as BuiltPayload>::Primitives as NodePrimitives>::Block,
        >,
        _bal: Option<Bytes>,
    ) -> Self::ExecutionData {
        HlExecutionData(block.into_block())
    }
}
