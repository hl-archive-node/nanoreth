use crate::{
    chainspec::HlChainSpec,
    hardforks::HlHardforks,
    node::{HlBlock, HlPrimitives},
};
use alloy_consensus::BlockHeader;
use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::B256;
use alloy_rpc_types_engine::PayloadError;
use reth::{
    api::{FullNodeComponents, NodeTypes},
    builder::{AddOnsContext, rpc::PayloadValidatorBuilder},
};
use reth_engine_primitives::{ExecutionPayload, PayloadValidator};
use reth_payload_primitives::NewPayloadError;
use reth_primitives_traits::SealedBlock;
use reth_primitives_traits::Block as _;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::payload::HlPayloadTypes;
use crate::node::engine::HlBuiltPayload;
use reth_payload_primitives::BuiltPayload;

#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct HlPayloadValidatorBuilder;

impl<Node, Types> PayloadValidatorBuilder<Node> for HlPayloadValidatorBuilder
where
    Types: NodeTypes<ChainSpec = HlChainSpec, Payload = HlPayloadTypes, Primitives = HlPrimitives>,
    Node: FullNodeComponents<Types = Types>,
{
    type Validator = HlPayloadValidator;

    async fn build(self, ctx: &AddOnsContext<'_, Node>) -> eyre::Result<Self::Validator> {
        Ok(HlPayloadValidator::new(Arc::new(ctx.config.chain.clone().as_ref().clone())))
    }
}

/// Validator for HyperEVM engine API.
#[derive(Debug, Clone)]
pub struct HlPayloadValidator {
    inner: HlExecutionPayloadValidator<HlChainSpec>,
}

impl HlPayloadValidator {
    /// Instantiates a new validator.
    pub fn new(chain_spec: Arc<HlChainSpec>) -> Self {
        Self { inner: HlExecutionPayloadValidator { inner: chain_spec } }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HlExecutionData(pub HlBlock);

impl ExecutionPayload for HlExecutionData {
    fn parent_hash(&self) -> B256 {
        self.0.header.parent_hash()
    }

    fn block_hash(&self) -> B256 {
        self.0.header.hash_slow()
    }

    fn block_number(&self) -> u64 {
        self.0.header.number()
    }

    fn withdrawals(&self) -> Option<&Vec<Withdrawal>> {
        None
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        None
    }

    fn timestamp(&self) -> u64 {
        self.0.header.timestamp()
    }

    fn gas_used(&self) -> u64 {
        self.0.header.gas_used()
    }

    fn gas_limit(&self) -> u64 {
        self.0.header.gas_limit()
    }

    fn block_access_list(&self) -> Option<&alloy_primitives::Bytes> {
        None
    }

    fn transaction_count(&self) -> usize {
        self.0.body.transactions.len()
    }

    fn slot_number(&self) -> Option<u64> {
        None
    }
}

impl From<HlBuiltPayload> for HlExecutionData {
    fn from(value: HlBuiltPayload) -> Self {
        Self(value.block().clone().into_block())
    }
}

impl PayloadValidator<HlPayloadTypes> for HlPayloadValidator {
    type Block = HlBlock;

    fn convert_payload_to_block(
        &self,
        payload: HlExecutionData,
    ) -> Result<SealedBlock<Self::Block>, NewPayloadError> {
        self.inner.ensure_well_formed_payload(payload).map_err(NewPayloadError::other)
    }
}

/// Execution payload validator.
#[derive(Clone, Debug)]
pub struct HlExecutionPayloadValidator<ChainSpec> {
    /// Chain spec to validate against.
    #[allow(unused)]
    inner: Arc<ChainSpec>,
}

impl<ChainSpec> HlExecutionPayloadValidator<ChainSpec>
where
    ChainSpec: HlHardforks,
{
    pub fn ensure_well_formed_payload(
        &self,
        payload: HlExecutionData,
    ) -> Result<SealedBlock<HlBlock>, PayloadError> {
        let block = payload.0;

        let expected_hash = block.header.hash_slow();

        // First parse the block
        let sealed_block = block.seal_slow();

        // Ensure the hash included in the payload matches the block hash
        if expected_hash != sealed_block.hash() {
            return Err(PayloadError::BlockHash {
                execution: sealed_block.hash(),
                consensus: expected_hash,
            });
        }

        Ok(sealed_block)
    }
}
