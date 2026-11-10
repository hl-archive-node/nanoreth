use crate::{chainspec::TESTNET_CHAIN_ID, node::types::BlockAndReceipts};
use std::{collections::HashMap, io::Read};

/// lz4-compressed JSON arrays of `BlockAndReceipts` missing from the upstream testnet block
/// sources:
/// - `testnet_gaps.json.lz4`: 50927214..=50929512
/// - `testnet_gaps_69038325.json.lz4`: 69038325..=69071492, rebuilt with `reth-hl rpc-to-blocks`
///   from another nanoreth node's RPC (hash chain pinned to the S3 blocks on both sides).
static TESTNET_GAPS_LZ4: &[&[u8]] =
    &[include_bytes!("testnet_gaps.json.lz4"), include_bytes!("testnet_gaps_69038325.json.lz4")];

/// Returns hardcoded testnet gap blocks keyed by block number.
/// These are blocks missing from the upstream testnet block sources.
/// Returns an empty map for non-testnet chains.
pub(crate) fn testnet_gap_blocks(chain_id: u64) -> HashMap<u64, BlockAndReceipts> {
    if chain_id != TESTNET_CHAIN_ID {
        return HashMap::new();
    }
    TESTNET_GAPS_LZ4
        .iter()
        .flat_map(|lz4| {
            let mut decoder = lz4_flex::frame::FrameDecoder::new(*lz4);
            let mut json = String::new();
            decoder.read_to_string(&mut json).expect("failed to decompress testnet gap blocks");
            serde_json::from_str::<Vec<BlockAndReceipts>>(&json)
                .expect("invalid testnet gap blocks json")
        })
        .map(|b| (b.number(), b))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::b256;

    /// Each embedded range is complete, chains by parent hash, and links to the S3 blocks
    /// around it (hashes of the last block before and the parent hash of the first block after).
    #[test]
    fn embedded_testnet_gaps_chain() {
        let blocks = testnet_gap_blocks(TESTNET_CHAIN_ID);
        for (first, last, before, after_parent) in [(
            69_038_325u64,
            69_071_492u64,
            b256!("718af98a59e22cfbaa9fd6ccd124f8fc59d52947eca59ce34f54032ae4ed9b25"),
            b256!("3051f88ba9a3c9960b10aa5a133b8d50f26ae230a94b29751261e0b471a41f8b"),
        )] {
            let mut parent = before;
            for n in first..=last {
                let b = blocks.get(&n).unwrap_or_else(|| panic!("missing block {n}"));
                assert_eq!(b.parent_hash(), parent, "block {n} parent");
                parent = b.hash();
            }
            assert_eq!(parent, after_parent, "range {first}..={last} end");
        }
        assert!(testnet_gap_blocks(1).is_empty());
    }
}
