use alloy_primitives::{address, Address};
use reth_evm::block::BlockExecutionError;
use revm::{primitives::HashMap, state::Account};

/// Applies storage patches to the state after a transaction is executed.
/// See https://github.com/hyperliquid-dex/hyper-evm-sync/commit/39047242b6260f7764527a2f5057dd9c3a75aa89 for more details.
static MAINNET_PATCHES_AFTER_TX: &[(u64, u64, bool, Address)] = &[
    (1_467_569, 0, false, address!("0x33f6fe38c55cb100ce27b3138e5d2d041648364f")),
    (1_467_631, 0, false, address!("0x33f6fe38c55cb100ce27b3138e5d2d041648364f")),
    (1_499_313, 2, false, address!("0xe27bfc0a812b38927ff646f24af9149f45deb550")),
    (1_499_406, 0, false, address!("0xe27bfc0a812b38927ff646f24af9149f45deb550")),
    (1_499_685, 0, false, address!("0xfee3932b75a87e86930668a6ab3ed43b404c8a30")),
    (1_514_843, 0, false, address!("0x723e5fbbeed025772a91240fd0956a866a41a603")),
    (1_514_936, 0, false, address!("0x723e5fbbeed025772a91240fd0956a866a41a603")),
    (1_530_529, 2, false, address!("0xa694e8fd8f4a177dd23636d838e9f1fb2138d87a")),
    (1_530_622, 2, false, address!("0xa694e8fd8f4a177dd23636d838e9f1fb2138d87a")),
    (1_530_684, 3, false, address!("0xa694e8fd8f4a177dd23636d838e9f1fb2138d87a")),
    (1_530_777, 3, false, address!("0xa694e8fd8f4a177dd23636d838e9f1fb2138d87a")),
    (1_530_839, 2, false, address!("0x692a343fc401a7755f8fc2facf61af426adaf061")),
    (1_530_901, 0, false, address!("0xfd9716f16596715ce765dabaee11787870e04b8a")),
    (1_530_994, 3, false, address!("0xfd9716f16596715ce765dabaee11787870e04b8a")),
    (1_531_056, 4, false, address!("0xdc67c2b8349ca20f58760e08371fc9271e82b5a4")),
    (1_531_149, 0, false, address!("0xdc67c2b8349ca20f58760e08371fc9271e82b5a4")),
    (1_531_211, 3, false, address!("0xdc67c2b8349ca20f58760e08371fc9271e82b5a4")),
    (1_531_366, 1, false, address!("0x9a90a517d27a9e60e454c96fefbbe94ff244ed6f")),
];

pub(crate) fn patch_mainnet_after_tx(
    block_number: u64,
    tx_index: u64,
    is_system_tx: bool,
    changes: &mut HashMap<Address, Account>,
) -> Result<(), BlockExecutionError> {
    if MAINNET_PATCHES_AFTER_TX.is_empty() {
        return Ok(());
    }

    let first = MAINNET_PATCHES_AFTER_TX.first().unwrap().0;
    let last = MAINNET_PATCHES_AFTER_TX.last().unwrap().0;
    if first > block_number || last < block_number {
        return Ok(());
    }

    for (block_num, idx, is_system, address) in MAINNET_PATCHES_AFTER_TX {
        if block_number == *block_num && tx_index == *idx && is_system_tx == *is_system {
            changes.remove(address);
        }
    }

    Ok(())
}
