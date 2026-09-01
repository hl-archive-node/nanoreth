use crate::chainspec::TESTNET_CHAIN_ID;
use alloy_eips::BlockNumHash;
use alloy_primitives::{B256, BlockNumber};
use reth_provider::{BlockIdReader, BlockNumReader, ProviderError};
use std::cmp::Ordering;

/// Number of canonical blocks kept reorgable behind the chain head.
pub(crate) const FINALIZATION_DEPTH: u64 = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FinalizationPolicy {
    Immediate,
    Depth(u64),
}

impl FinalizationPolicy {
    pub(crate) fn from_chain_id(chain_id: u64) -> Self {
        match chain_id {
            TESTNET_CHAIN_ID => Self::Depth(FINALIZATION_DEPTH),
            _ => Self::Immediate,
        }
    }

    /// Returns the finalization height without moving behind persisted finality.
    pub(crate) fn finalized_number(
        self,
        head_number: BlockNumber,
        persisted_finalized: Option<BlockNumber>,
    ) -> Option<BlockNumber> {
        let candidate = match self {
            Self::Immediate => Some(head_number),
            Self::Depth(depth) => head_number.checked_sub(depth),
        };
        match (candidate, persisted_finalized) {
            (Some(candidate), Some(persisted)) => Some(candidate.max(persisted)),
            (candidate, persisted) => candidate.or(persisted),
        }
    }
}

/// Errors that can occur in Hl consensus
#[derive(Debug, thiserror::Error)]
pub enum HlConsensusErr {
    /// Error from the provider
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// Head block hash not found
    #[error("Head block hash not found")]
    HeadHashNotFound,
}

/// Hl consensus implementation
pub struct HlConsensus<P> {
    /// The provider for reading block information
    pub provider: P,
    pub(crate) finalization_policy: FinalizationPolicy,
}

impl<P> HlConsensus<P>
where
    P: BlockIdReader + BlockNumReader + Clone,
{
    /// Determines the head block hash according to Hl consensus rules:
    /// 1. Follow the highest block number
    /// 2. For same height blocks, pick the one with lower hash
    pub(crate) fn canonical_head(
        &self,
        hash: B256,
        number: BlockNumber,
    ) -> Result<(B256, B256), HlConsensusErr> {
        let current_head = self.provider.best_block_number()?;
        let current_hash =
            self.provider.block_hash(current_head)?.ok_or(HlConsensusErr::HeadHashNotFound)?;

        match number.cmp(&current_head) {
            Ordering::Greater => Ok((hash, current_hash)),
            Ordering::Equal => Ok((hash.min(current_hash), current_hash)),
            Ordering::Less => Ok((current_hash, current_hash)),
        }
    }

    /// Returns the canonical hash that is old enough to mark safe and finalized.
    pub(crate) fn finalized_hash(
        &self,
        head_hash: B256,
        head_number: BlockNumber,
    ) -> Result<B256, HlConsensusErr> {
        let persisted = self.provider.finalized_block_num_hash()?;
        let Some(finalized_number) = self
            .finalization_policy
            .finalized_number(head_number, persisted.map(|block| block.number))
        else {
            return Ok(B256::ZERO);
        };

        if finalized_number == head_number {
            return Ok(head_hash);
        }

        if let Some(BlockNumHash { number, hash }) = persisted
            && number == finalized_number
        {
            return Ok(hash);
        }

        Ok(self.provider.block_hash(finalized_number)?.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;
    use reth_chainspec::ChainInfo;
    use reth_provider::{BlockHashReader, ProviderResult};
    use std::collections::HashMap;

    fn consensus(provider: MockProvider) -> HlConsensus<MockProvider> {
        HlConsensus { provider, finalization_policy: FinalizationPolicy::Depth(FINALIZATION_DEPTH) }
    }

    #[derive(Clone)]
    struct MockProvider {
        blocks: HashMap<BlockNumber, B256>,
        head_number: BlockNumber,
        head_hash: B256,
        finalized: Option<BlockNumHash>,
    }

    impl MockProvider {
        fn new(head_number: BlockNumber, head_hash: B256) -> Self {
            let mut blocks = HashMap::new();
            blocks.insert(head_number, head_hash);
            Self { blocks, head_number, head_hash, finalized: None }
        }
    }

    impl BlockIdReader for MockProvider {
        fn pending_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
            Ok(None)
        }

        fn safe_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
            Ok(self.finalized)
        }

        fn finalized_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
            Ok(self.finalized)
        }
    }

    impl BlockHashReader for MockProvider {
        fn block_hash(&self, number: BlockNumber) -> Result<Option<B256>, ProviderError> {
            Ok(self.blocks.get(&number).copied())
        }

        fn canonical_hashes_range(
            &self,
            _start: BlockNumber,
            _end: BlockNumber,
        ) -> Result<Vec<B256>, ProviderError> {
            Ok(vec![])
        }
    }

    impl BlockNumReader for MockProvider {
        fn chain_info(&self) -> Result<ChainInfo, ProviderError> {
            Ok(ChainInfo { best_hash: self.head_hash, best_number: self.head_number })
        }

        fn best_block_number(&self) -> Result<BlockNumber, ProviderError> {
            Ok(self.head_number)
        }

        fn last_block_number(&self) -> Result<BlockNumber, ProviderError> {
            Ok(self.head_number)
        }

        fn block_number(&self, hash: B256) -> Result<Option<BlockNumber>, ProviderError> {
            Ok(self.blocks.iter().find_map(|(num, h)| (*h == hash).then_some(*num)))
        }
    }

    #[test]
    fn test_canonical_head() {
        let hash1 = B256::from_slice(&hex!(
            "1111111111111111111111111111111111111111111111111111111111111111"
        ));
        let hash2 = B256::from_slice(&hex!(
            "2222222222222222222222222222222222222222222222222222222222222222"
        ));

        let test_cases = [
            ((hash1, 2, 1, hash2), hash1), // Higher block wins
            ((hash1, 1, 2, hash2), hash2), // Lower block stays
            ((hash1, 1, 1, hash2), hash1), // Same height, lower hash wins
            ((hash2, 1, 1, hash1), hash1), // Same height, lower hash stays
        ];

        for ((curr_hash, curr_num, head_num, head_hash), expected) in test_cases {
            let provider = MockProvider::new(head_num, head_hash);
            let consensus = consensus(provider);
            let (head_block_hash, current_hash) =
                consensus.canonical_head(curr_hash, curr_num).unwrap();
            assert_eq!(head_block_hash, expected);
            assert_eq!(current_hash, head_hash);
        }
    }

    #[test]
    fn finalization_waits_for_full_depth() {
        let head_hash = B256::repeat_byte(1);
        let consensus = consensus(MockProvider::new(255, head_hash));

        assert_eq!(consensus.finalized_hash(head_hash, 255).unwrap(), B256::ZERO);
    }

    #[test]
    fn finalization_uses_canonical_hash_at_depth() {
        let finalized_hash = B256::repeat_byte(1);
        let head_hash = B256::repeat_byte(2);
        let mut provider = MockProvider::new(512, head_hash);
        provider.blocks.insert(256, finalized_hash);
        let consensus = consensus(provider);

        assert_eq!(consensus.finalized_hash(head_hash, 512).unwrap(), finalized_hash);
    }

    #[test]
    fn finalization_stays_zero_when_target_is_before_available_history() {
        let head_hash = B256::repeat_byte(2);
        let consensus = consensus(MockProvider::new(50_000_512, head_hash));

        assert_eq!(consensus.finalized_hash(head_hash, 50_000_512).unwrap(), B256::ZERO);
    }

    #[test]
    fn finalization_does_not_regress_during_depth_transition() {
        let finalized = BlockNumHash::new(512, B256::repeat_byte(1));
        let head_hash = B256::repeat_byte(2);
        let mut provider = MockProvider::new(513, head_hash);
        provider.finalized = Some(finalized);
        let consensus = consensus(provider);

        assert_eq!(consensus.finalized_hash(head_hash, 513).unwrap(), finalized.hash);
    }

    #[test]
    fn finalization_advances_after_transition() {
        let finalized = BlockNumHash::new(512, B256::repeat_byte(1));
        let advanced_hash = B256::repeat_byte(2);
        let mut provider = MockProvider::new(769, B256::repeat_byte(3));
        provider.finalized = Some(finalized);
        provider.blocks.insert(513, advanced_hash);
        let head_hash = provider.head_hash;
        let consensus = consensus(provider);

        assert_eq!(consensus.finalized_hash(head_hash, 769).unwrap(), advanced_hash);
    }

    #[test]
    fn finalization_floor_is_preserved_when_source_head_is_behind() {
        let policy = FinalizationPolicy::Depth(FINALIZATION_DEPTH);
        assert_eq!(policy.finalized_number(500, Some(512)), Some(512));
    }

    #[test]
    fn reorg_depth_is_only_enabled_on_testnet() {
        assert_eq!(
            FinalizationPolicy::from_chain_id(TESTNET_CHAIN_ID),
            FinalizationPolicy::Depth(FINALIZATION_DEPTH)
        );
        assert_eq!(
            FinalizationPolicy::from_chain_id(crate::chainspec::MAINNET_CHAIN_ID),
            FinalizationPolicy::Immediate
        );
    }

    #[test]
    fn immediate_finalization_uses_unpersisted_head_hash() {
        let head_hash = B256::repeat_byte(2);
        let consensus = HlConsensus {
            provider: MockProvider::new(10, B256::repeat_byte(1)),
            finalization_policy: FinalizationPolicy::Immediate,
        };

        assert_eq!(consensus.finalized_hash(head_hash, 11).unwrap(), head_hash);
    }
}
