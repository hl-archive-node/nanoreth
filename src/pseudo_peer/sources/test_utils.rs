use crate::node::types::{BlockAndReceipts, EvmBlock, ReadPrecompileCalls, reth_compat};
use alloy_consensus::{BlockBody, Header};
use alloy_primitives::{Address, B64, B256, Bloom, Bytes, U256};
use std::{fs::File, io::Write, path::Path};

pub fn block(number: u64, marker: u8) -> BlockAndReceipts {
    block_with_parent(number, marker, B256::repeat_byte(marker.saturating_sub(1)))
}

pub fn block_with_parent(number: u64, marker: u8, parent_hash: B256) -> BlockAndReceipts {
    BlockAndReceipts {
        block: EvmBlock::Reth115(reth_compat::SealedBlock {
            header: reth_compat::SealedHeader {
                header: Header {
                    parent_hash,
                    ommers_hash: B256::ZERO,
                    beneficiary: Address::ZERO,
                    state_root: B256::repeat_byte(marker),
                    transactions_root: B256::ZERO,
                    receipts_root: B256::repeat_byte(marker),
                    logs_bloom: Bloom::ZERO,
                    difficulty: U256::ZERO,
                    number,
                    gas_limit: 0,
                    gas_used: 0,
                    timestamp: number,
                    extra_data: Bytes::from(vec![marker]),
                    mix_hash: B256::ZERO,
                    nonce: B64::ZERO,
                    base_fee_per_gas: None,
                    withdrawals_root: None,
                    blob_gas_used: None,
                    excess_blob_gas: None,
                    parent_beacon_block_root: None,
                    requests_hash: None,
                },
                hash: B256::repeat_byte(marker),
            },
            body: BlockBody { transactions: vec![], ommers: vec![], withdrawals: None },
        }),
        receipts: vec![],
        system_txs: vec![],
        read_precompile_calls: ReadPrecompileCalls(vec![]),
        highest_precompile_address: Some(Address::repeat_byte(marker)),
    }
}

pub fn encode(blocks: &[BlockAndReceipts]) -> Vec<u8> {
    let mut encoder = lz4_flex::frame::FrameEncoder::new(Vec::new());
    rmp_serde::encode::write_named(&mut encoder, blocks).unwrap();
    encoder.finish().unwrap()
}

pub fn write_local(root: &Path, block: &BlockAndReceipts) {
    let path = root.join(super::utils::rmp_path(block.number()));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = File::create(path).unwrap();
    file.write_all(&encode(std::slice::from_ref(block))).unwrap();
}
