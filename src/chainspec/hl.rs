use alloy_chains::{Chain, NamedChain};
use alloy_primitives::{b256, Address, Bytes, B256, B64, U256};
use reth_chainspec::{ChainHardforks, ChainSpec, EthereumHardfork, ForkCondition, Hardfork};
use reth_primitives::{Header, SealedHeader};
use std::sync::LazyLock;

static GENESIS_HASH: B256 =
    b256!("d8fcc13b6a195b88b7b2da3722ff6cad767b13a8c1e9ffb1c73aa9d216d895f0");

pub static HL_HARDFORKS: LazyLock<ChainHardforks> = LazyLock::new(|| {
    ChainHardforks::new(vec![
        (EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Dao.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::London.boxed(), ForkCondition::Block(0)),
        (
            EthereumHardfork::Paris.boxed(),
            ForkCondition::TTD {
                activation_block_number: 0,
                fork_block: None,
                total_difficulty: U256::ZERO,
            },
        ),
        (EthereumHardfork::Shanghai.boxed(), ForkCondition::Timestamp(0)),
        (EthereumHardfork::Cancun.boxed(), ForkCondition::Timestamp(0)),
    ])
});

pub fn hl_chainspec(chain: Chain, genesis: &'static str) -> ChainSpec {
    ChainSpec {
        chain,
        genesis: serde_json::from_str(genesis).expect("Can't deserialize Hyperliquid genesis json"),
        genesis_header: empty_genesis_header(),
        paris_block_and_final_difficulty: Some((0, U256::from(0))),
        hardforks: HL_HARDFORKS.clone(),
        prune_delete_limit: 10000,
        ..Default::default()
    }
}

/// The Hyperliqiud Mainnet spec
pub fn hl_mainnet() -> ChainSpec {
    hl_chainspec(Chain::from_named(NamedChain::Hyperliquid), include_str!("genesis.json"))
}

/// The Hyperliqiud Testnet spec
pub fn hl_testnet() -> ChainSpec {
    // Note: Testnet sync starts from snapshotted state [1] instead of genesis block.
    // So the `alloc` field is not used, which makes it fine to reuse mainnet genesis file.
    hl_chainspec(Chain::from_id_unchecked(998), include_str!("genesis.json"))
}

/// Empty genesis header for Hyperliquid Mainnet.
///
/// The exact value is not known per se, but the parent hash of block 1 is known to be
/// [GENESIS_HASH].
fn empty_genesis_header() -> SealedHeader {
    SealedHeader::new(
        Header {
            parent_hash: B256::ZERO,
            number: 0,
            timestamp: 0,
            transactions_root: B256::ZERO,
            receipts_root: B256::ZERO,
            state_root: B256::ZERO,
            gas_used: 0,
            gas_limit: 0x1c9c380,
            difficulty: U256::ZERO,
            mix_hash: B256::ZERO,
            extra_data: Bytes::new(),
            nonce: B64::ZERO,
            ommers_hash: B256::ZERO,
            beneficiary: Address::ZERO,
            logs_bloom: Default::default(),
            base_fee_per_gas: Some(0),
            withdrawals_root: Some(B256::ZERO),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            parent_beacon_block_root: Some(B256::ZERO),
            requests_hash: Some(B256::ZERO),
        },
        GENESIS_HASH,
    )
}
