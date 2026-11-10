//! Hard forks of HyperEVM.
#![allow(unused)]
pub mod hl;

use hl::HlHardfork;
use reth_chainspec::{EthereumHardforks, ForkCondition};
use std::sync::Arc;

/// Extends [`EthereumHardforks`] with HyperEVM hardfork helpers.
pub trait HlHardforks: EthereumHardforks {
    fn hl_fork_activation(&self, fork: HlHardfork) -> ForkCondition;

    fn is_hl_v2_active_at_block(&self, block_number: u64) -> bool {
        self.hl_fork_activation(HlHardfork::V2).active_at_block(block_number)
    }
}

impl<T: HlHardforks> HlHardforks for Arc<T> {
    fn hl_fork_activation(&self, fork: HlHardfork) -> ForkCondition {
        (**self).hl_fork_activation(fork)
    }
}
