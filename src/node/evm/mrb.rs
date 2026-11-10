//! Hyperliquid MRB precompile resource accounting.

use crate::{
    chainspec::{MAINNET_CHAIN_ID, TESTNET_CHAIN_ID},
    evm::spec::HlSpecId,
};
use alloy_primitives::Address;
use revm::precompile::{blake2, bn254, kzg_point_evaluation, modexp, secp256k1};
use std::sync::OnceLock;

/// Mainnet MRB activation height announced by Hyperliquid.
pub const MAINNET_ACTIVATION_BLOCK: u64 = 50_988_232;
/// Testnet MRB activation height announced by Hyperliquid.
pub const TESTNET_ACTIVATION_BLOCK: u64 = 69_259_950;

static ACTIVATION_BLOCK: OnceLock<u64> = OnceLock::new();

/// Overrides the announced activation height. `None` keeps the announced height.
///
/// The height can be set once per process; setting the same height again is a no-op.
pub fn set_activation_block(block_number: Option<u64>) -> eyre::Result<()> {
    let Some(block_number) = block_number else { return Ok(()) };
    let current = *ACTIVATION_BLOCK.get_or_init(|| block_number);
    eyre::ensure!(
        current == block_number,
        "MRB activation block is already fixed at {current}, refusing replacement with {block_number}"
    );
    Ok(())
}

pub fn activation_block() -> Option<u64> {
    ACTIVATION_BLOCK.get().copied()
}

pub const fn scheduled_activation_block(chain_id: u64) -> Option<u64> {
    match chain_id {
        MAINNET_CHAIN_ID => Some(MAINNET_ACTIVATION_BLOCK),
        TESTNET_CHAIN_ID => Some(TESTNET_ACTIVATION_BLOCK),
        _ => None,
    }
}

/// The override when set, otherwise the height announced for `chain_id`.
pub fn activation_block_for_chain(chain_id: u64) -> Option<u64> {
    activation_block().or(scheduled_activation_block(chain_id))
}

/// The spec that executes `block_number` on `chain_id`.
pub fn spec_at(chain_id: u64, block_number: u64) -> HlSpecId {
    if is_active_at(block_number, activation_block_for_chain(chain_id)) {
        HlSpecId::V2
    } else {
        HlSpecId::V1
    }
}

pub const fn is_active_at(block_number: u64, activation_block: Option<u64>) -> bool {
    match activation_block {
        Some(activation_block) => block_number >= activation_block,
        None => false,
    }
}

pub fn precompile_multiplier(address: Address) -> u64 {
    if address == *secp256k1::ECRECOVER.address() {
        5
    } else if address == *modexp::BERLIN.address()
        || address == bn254::add::ADDRESS
        || address == bn254::mul::ADDRESS
        || address == bn254::pair::ADDRESS
        || address == *blake2::FUN.address()
    {
        10
    } else if address == kzg_point_evaluation::ADDRESS {
        40
    } else {
        1
    }
}

pub fn affected_precompile_addresses() -> [Address; 7] {
    [
        *secp256k1::ECRECOVER.address(),
        *modexp::BERLIN.address(),
        bn254::add::ADDRESS,
        bn254::mul::ADDRESS,
        bn254::pair::ADDRESS,
        *blake2::FUN.address(),
        kzg_point_evaluation::ADDRESS,
    ]
}

/// Activation height shared by every test that sets it, since the height is fixed per process.
#[cfg(test)]
pub(crate) const TEST_ACTIVATION_BLOCK: u64 = 1_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_uses_an_inclusive_block_threshold() {
        assert!(!is_active_at(99, Some(100)));
        assert!(is_active_at(100, Some(100)));
        assert!(!is_active_at(u64::MAX, None));
    }

    #[test]
    fn activation_block_is_immutable_once_set() {
        set_activation_block(None).unwrap();
        set_activation_block(Some(TEST_ACTIVATION_BLOCK)).unwrap();
        set_activation_block(Some(TEST_ACTIVATION_BLOCK)).unwrap();
        set_activation_block(None).unwrap();
        assert!(set_activation_block(Some(TEST_ACTIVATION_BLOCK + 1)).is_err());
        assert_eq!(activation_block(), Some(TEST_ACTIVATION_BLOCK));
    }

    #[test]
    fn announced_heights_are_scheduled_per_chain() {
        assert_eq!(scheduled_activation_block(MAINNET_CHAIN_ID), Some(50_988_232));
        assert_eq!(scheduled_activation_block(TESTNET_CHAIN_ID), Some(69_259_950));
        assert_eq!(scheduled_activation_block(1), None);
    }

    #[test]
    fn spec_switches_at_the_chain_activation_block() {
        // Other tests may set the process-wide override, so read the effective height.
        let activation = activation_block_for_chain(TESTNET_CHAIN_ID).unwrap();
        assert_eq!(spec_at(TESTNET_CHAIN_ID, activation - 1), HlSpecId::V1);
        assert_eq!(spec_at(TESTNET_CHAIN_ID, activation), HlSpecId::V2);
    }

    #[test]
    fn multiplier_table_uses_revm_precompile_addresses() {
        assert_eq!(precompile_multiplier(*secp256k1::ECRECOVER.address()), 5);
        assert_eq!(precompile_multiplier(*modexp::BERLIN.address()), 10);
        assert_eq!(precompile_multiplier(bn254::add::ADDRESS), 10);
        assert_eq!(precompile_multiplier(bn254::mul::ADDRESS), 10);
        assert_eq!(precompile_multiplier(bn254::pair::ADDRESS), 10);
        assert_eq!(precompile_multiplier(*blake2::FUN.address()), 10);
        assert_eq!(precompile_multiplier(kzg_point_evaluation::ADDRESS), 40);
        assert_eq!(precompile_multiplier(Address::ZERO), 1);
    }
}
