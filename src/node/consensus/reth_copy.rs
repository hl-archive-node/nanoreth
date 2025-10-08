//! Copy of reth codebase.

use alloy_consensus::{proofs::calculate_receipt_root, BlockHeader, TxReceipt};
use alloy_eips::eip7685::Requests;
use alloy_primitives::{Bloom, B256};
use reth::consensus::ConsensusError;
use reth_chainspec::EthereumHardforks;
use reth_primitives::{gas_spent_by_transactions, GotExpected, RecoveredBlock};
use reth_primitives_traits::{Block, Receipt as ReceiptTrait};

pub fn validate_block_post_execution<B, R, ChainSpec>(
    block: &RecoveredBlock<B>,
    chain_spec: &ChainSpec,
    receipts: &[R],
    requests: &Requests,
) -> Result<(), ConsensusError>
where
    B: Block,
    R: ReceiptTrait,
    ChainSpec: EthereumHardforks,
{
    // Copy of reth's validate_block_post_execution
    // Differences:
    // - Filter out system transactions for receipts check

    // Check if gas used matches the value set in header.
    let cumulative_gas_used =
        receipts.last().map(|receipt| receipt.cumulative_gas_used()).unwrap_or(0);
    if block.header().gas_used() != cumulative_gas_used {
        return Err(ConsensusError::BlockGasUsed {
            gas: GotExpected { got: cumulative_gas_used, expected: block.header().gas_used() },
            gas_spent_by_tx: gas_spent_by_transactions(receipts),
        });
    }

    // Before Byzantium, receipts contained state root that would mean that expensive
    // operation as hashing that is required for state root got calculated in every
    // transaction This was replaced with is_success flag.
    // See more about EIP here: https://eips.ethereum.org/EIPS/eip-658
    if chain_spec.is_byzantium_active_at_block(block.header().number()) {
        let receipts_for_root =
            receipts.iter().filter(|&r| r.cumulative_gas_used() != 0).cloned().collect::<Vec<_>>();
        if let Err(error) = verify_receipts(
            block.header().receipts_root(),
            block.header().logs_bloom(),
            &receipts_for_root,
        ) {
            tracing::debug!(%error, ?receipts, "receipts verification failed");
            return Err(error);
        }
    }

    // Validate that the header requests hash matches the calculated requests hash
    if chain_spec.is_prague_active_at_timestamp(block.header().timestamp()) {
        let Some(header_requests_hash) = block.header().requests_hash() else {
            return Err(ConsensusError::RequestsHashMissing);
        };
        let requests_hash = requests.requests_hash();
        if requests_hash != header_requests_hash {
            return Err(ConsensusError::BodyRequestsHashDiff(
                GotExpected::new(requests_hash, header_requests_hash).into(),
            ));
        }
    }

    Ok(())
}

/// Calculate the receipts root, and compare it against the expected receipts root and logs
/// bloom.
pub(super) fn verify_receipts<R: ReceiptTrait>(
    expected_receipts_root: B256,
    expected_logs_bloom: Bloom,
    receipts: &[R],
) -> Result<(), ConsensusError> {
    // Calculate receipts root.
    let receipts_with_bloom = receipts.iter().map(TxReceipt::with_bloom_ref).collect::<Vec<_>>();
    let receipts_root = calculate_receipt_root(&receipts_with_bloom);

    // Calculate header logs bloom.
    let logs_bloom = receipts_with_bloom.iter().fold(Bloom::ZERO, |bloom, r| bloom | r.bloom_ref());

    compare_receipts_root_and_logs_bloom(
        receipts_root,
        logs_bloom,
        expected_receipts_root,
        expected_logs_bloom,
    )?;

    Ok(())
}

/// Compare the calculated receipts root with the expected receipts root, also compare
/// the calculated logs bloom with the expected logs bloom.
pub(super) fn compare_receipts_root_and_logs_bloom(
    calculated_receipts_root: B256,
    calculated_logs_bloom: Bloom,
    expected_receipts_root: B256,
    expected_logs_bloom: Bloom,
) -> Result<(), ConsensusError> {
    if calculated_receipts_root != expected_receipts_root {
        return Err(ConsensusError::BodyReceiptRootDiff(
            GotExpected { got: calculated_receipts_root, expected: expected_receipts_root }.into(),
        ));
    }

    if calculated_logs_bloom != expected_logs_bloom {
        return Err(ConsensusError::BodyBloomLogDiff(
            GotExpected { got: calculated_logs_bloom, expected: expected_logs_bloom }.into(),
        ));
    }

    Ok(())
}
