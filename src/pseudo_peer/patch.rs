use crate::{chainspec::TESTNET_CHAIN_ID, node::types::BlockAndReceipts};
use std::{collections::HashMap, io::Read};

static TESTNET_GAPS_LZ4: &[u8] = include_bytes!("testnet_gaps.json.lz4");

/// Returns hardcoded testnet gap blocks keyed by block number.
/// These are blocks missing from the upstream testnet block sources.
/// Returns an empty map for non-testnet chains.
pub(crate) fn testnet_gap_blocks(chain_id: u64) -> HashMap<u64, BlockAndReceipts> {
    if chain_id != TESTNET_CHAIN_ID {
        return HashMap::new();
    }
    let mut decoder = lz4_flex::frame::FrameDecoder::new(TESTNET_GAPS_LZ4);
    let mut json = String::new();
    decoder.read_to_string(&mut json).expect("failed to decompress testnet_gaps.json.lz4");
    let blocks: Vec<BlockAndReceipts> =
        serde_json::from_str(&json).expect("invalid testnet_gaps.json");
    blocks.into_iter().map(|b| (b.number(), b)).collect()
}
