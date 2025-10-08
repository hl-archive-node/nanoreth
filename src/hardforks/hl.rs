#![allow(unused)]
use alloy_chains::{Chain, NamedChain};
use core::any::Any;
use reth_chainspec::ForkCondition;
use reth_ethereum_forks::{hardfork, ChainHardforks, EthereumHardfork, Hardfork};

hardfork!(
    /// The name of a hl hardfork.
    ///
    /// When building a list of hardforks for a chain, it's still expected to mix with [`EthereumHardfork`].
    /// There is no name for these hardforks; just some bugfixes on the evm chain.
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    HlHardfork {
        /// Initial version
        V1,
    }
);
