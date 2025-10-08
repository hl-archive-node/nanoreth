use crate::{hardforks::HlHardforks, node::HlNode, HlBlock, HlBlockBody, HlPrimitives};
use alloy_consensus::Header;
use reth::{
    api::FullNodeTypes,
    beacon_consensus::EthBeaconConsensus,
    builder::{components::ConsensusBuilder, BuilderContext},
    consensus::{Consensus, ConsensusError, FullConsensus, HeaderValidator},
    consensus_common::validation::{
        validate_against_parent_4844, validate_against_parent_hash_number,
    },
};
use reth_chainspec::EthChainSpec;
use reth_primitives::{Receipt, RecoveredBlock, SealedBlock, SealedHeader};
use reth_primitives_traits::BlockHeader;
use reth_provider::BlockExecutionResult;
use std::sync::Arc;

/// A basic Hl consensus builder.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct HlConsensusBuilder;

impl<Node> ConsensusBuilder<Node> for HlConsensusBuilder
where
    Node: FullNodeTypes<Types = HlNode>,
{
    type Consensus = Arc<dyn FullConsensus<HlPrimitives, Error = ConsensusError>>;

    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        Ok(Arc::new(HlConsensus::new(ctx.chain_spec())))
    }
}

/// HL consensus implementation.
///
/// Provides basic checks as outlined in the execution specs.
#[derive(Debug, Clone)]
pub struct HlConsensus<ChainSpec> {
    inner: EthBeaconConsensus<ChainSpec>,
    chain_spec: Arc<ChainSpec>,
}

impl<ChainSpec> HlConsensus<ChainSpec>
where
    ChainSpec: EthChainSpec + HlHardforks,
{
    /// Create a new instance of [`HlConsensus`]
    pub fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self { inner: EthBeaconConsensus::new(chain_spec.clone()), chain_spec }
    }
}

/// Validates the timestamp against the parent to make sure it is in the past.
#[inline]
pub fn validate_against_parent_timestamp<H: BlockHeader>(
    header: &H,
    parent: &H,
) -> Result<(), ConsensusError> {
    // NOTE: HyperEVM allows the timestamp to be the same as the parent (big and small blocks)
    if header.timestamp() < parent.timestamp() {
        return Err(ConsensusError::TimestampIsInPast {
            parent_timestamp: parent.timestamp(),
            timestamp: header.timestamp(),
        });
    }
    Ok(())
}

impl<H, ChainSpec> HeaderValidator<H> for HlConsensus<ChainSpec>
where
    H: BlockHeader,
    ChainSpec: EthChainSpec<Header = H> + HlHardforks,
{
    fn validate_header(&self, header: &SealedHeader<H>) -> Result<(), ConsensusError> {
        self.inner.validate_header(header)
    }

    fn validate_header_against_parent(
        &self,
        header: &SealedHeader<H>,
        parent: &SealedHeader<H>,
    ) -> Result<(), ConsensusError> {
        validate_against_parent_hash_number(header.header(), parent)?;

        validate_against_parent_timestamp(header.header(), parent.header())?;

        // validate_against_parent_eip1559_base_fee(
        //     header.header(),
        //     parent.header(),
        //     &self.chain_spec,
        // )?;

        // ensure that the blob gas fields for this block
        if let Some(blob_params) = self.chain_spec.blob_params_at_timestamp(header.timestamp()) {
            validate_against_parent_4844(header.header(), parent.header(), blob_params)?;
        }

        Ok(())
    }
}

impl<ChainSpec> Consensus<HlBlock> for HlConsensus<ChainSpec>
where
    ChainSpec: EthChainSpec<Header = Header> + HlHardforks,
{
    type Error = ConsensusError;

    fn validate_body_against_header(
        &self,
        body: &HlBlockBody,
        header: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        Consensus::<HlBlock>::validate_body_against_header(&self.inner, body, header)
    }

    fn validate_block_pre_execution(
        &self,
        _block: &SealedBlock<HlBlock>,
    ) -> Result<(), ConsensusError> {
        // Check ommers hash
        // let ommers_hash = block.body().calculate_ommers_root();
        // if Some(block.ommers_hash()) != ommers_hash {
        //     return Err(ConsensusError::BodyOmmersHashDiff(
        //         GotExpected {
        //             got: ommers_hash.unwrap_or(EMPTY_OMMER_ROOT_HASH),
        //             expected: block.ommers_hash(),
        //         }
        //         .into(),
        //     ))
        // }

        // // Check transaction root
        // if let Err(error) = block.ensure_transaction_root_valid() {
        //     return Err(ConsensusError::BodyTransactionRootDiff(error.into()))
        // }

        // if self.chain_spec.is_cancun_active_at_timestamp(block.timestamp()) {
        //     validate_cancun_gas(block)?;
        // } else {
        //     return Ok(())
        // }

        Ok(())
    }
}

mod reth_copy;

impl<ChainSpec> FullConsensus<HlPrimitives> for HlConsensus<ChainSpec>
where
    ChainSpec: EthChainSpec<Header = Header> + HlHardforks,
{
    fn validate_block_post_execution(
        &self,
        block: &RecoveredBlock<HlBlock>,
        result: &BlockExecutionResult<Receipt>,
    ) -> Result<(), ConsensusError> {
        reth_copy::validate_block_post_execution(
            block,
            &self.chain_spec,
            &result.receipts,
            &result.requests,
        )
    }
}
