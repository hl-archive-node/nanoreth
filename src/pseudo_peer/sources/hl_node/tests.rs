use super::*;
use crate::{
    node::types::{reth_compat, ReadPrecompileCalls},
    pseudo_peer::sources::{hl_node::scan::LocalBlockAndReceipts, LocalBlockSource},
};
use alloy_consensus::{BlockBody, Header};
use alloy_primitives::{Address, Bloom, Bytes, B256, B64, U256};
use std::{io::Write, time::Duration};

const DEFAULT_FALLBACK_THRESHOLD_FOR_TEST: Duration = Duration::from_millis(5000);

#[test]
fn test_datetime_from_path() {
    let path = Path::new("/home/username/hl/data/evm_block_and_receipts/hourly/20250731/4");
    let dt = TimeUtils::datetime_from_path(path).unwrap();
    println!("{dt:?}");
}

#[tokio::test]
async fn test_backfill() {
    let test_path = Path::new("/root/evm_block_and_receipts");
    if !test_path.exists() {
        return;
    }

    let cache = Arc::new(Mutex::new(LocalBlocksCache::new(CACHE_SIZE)));
    HlNodeBlockSource::try_backfill_local_blocks(test_path, &cache, 1000000).await.unwrap();

    let u_cache = cache.lock().await;
    assert_eq!(
        u_cache.get_path_for_height(9735058),
        Some(test_path.join(HOURLY_SUBDIR).join("20250729").join("22"))
    );
}

fn scan_result_from_single_block(block: BlockAndReceipts) -> scan::ScanResult {
    use crate::node::types::EvmBlock;
    let height = match &block.block {
        EvmBlock::Reth115(b) => b.header.header.number,
    };
    scan::ScanResult {
        path: PathBuf::from("/nonexistent-block"),
        next_expected_height: height + 1,
        new_blocks: vec![block],
        new_block_ranges: vec![height..=height],
    }
}

fn empty_block(number: u64, timestamp: u64, extra_data: &'static [u8]) -> LocalBlockAndReceipts {
    use crate::node::types::EvmBlock;
    LocalBlockAndReceipts(
        timestamp.to_string(),
        BlockAndReceipts {
            block: EvmBlock::Reth115(reth_compat::SealedBlock {
                header: reth_compat::SealedHeader {
                    header: Header {
                        parent_hash: B256::ZERO,
                        ommers_hash: B256::ZERO,
                        beneficiary: Address::ZERO,
                        state_root: B256::ZERO,
                        transactions_root: B256::ZERO,
                        receipts_root: B256::ZERO,
                        logs_bloom: Bloom::ZERO,
                        difficulty: U256::ZERO,
                        number,
                        gas_limit: 0,
                        gas_used: 0,
                        timestamp,
                        extra_data: Bytes::from_static(extra_data),
                        mix_hash: B256::ZERO,
                        nonce: B64::ZERO,
                        base_fee_per_gas: None,
                        withdrawals_root: None,
                        blob_gas_used: None,
                        excess_blob_gas: None,
                        parent_beacon_block_root: None,
                        requests_hash: None,
                    },
                    hash: B256::ZERO,
                },
                body: BlockBody { transactions: vec![], ommers: vec![], withdrawals: None },
            }),
            receipts: vec![],
            system_txs: vec![],
            read_precompile_calls: ReadPrecompileCalls(vec![]),
            highest_precompile_address: None,
        },
    )
}

fn setup_temp_dir_and_file() -> eyre::Result<(tempfile::TempDir, std::fs::File)> {
    let now = OffsetDateTime::now_utc();
    let temp_dir = tempfile::tempdir()?;
    let path = temp_dir
        .path()
        .join(HOURLY_SUBDIR)
        .join(TimeUtils::date_from_datetime(now))
        .join(format!("{}", now.hour()));
    std::fs::create_dir_all(path.parent().unwrap())?;
    Ok((temp_dir, std::fs::File::create(path)?))
}

struct BlockSourceHierarchy {
    block_source: HlNodeBlockSource,
    _temp_dir: tempfile::TempDir,
    file1: std::fs::File,
    current_block: LocalBlockAndReceipts,
    future_block_hl_node: LocalBlockAndReceipts,
    future_block_fallback: LocalBlockAndReceipts,
}

async fn setup_block_source_hierarchy() -> eyre::Result<BlockSourceHierarchy> {
    // Setup fallback block source
    let block_source_fallback = HlNodeBlockSource::new(
        BlockSourceBoxed::new(Box::new(LocalBlockSource::new("/nonexistent"))),
        HlNodeBlockSourceArgs {
            root: { PathBuf::from("/nonexistent") },
            fallback_threshold: DEFAULT_FALLBACK_THRESHOLD_FOR_TEST,
        },
        1000000,
    )
    .await;
    let block_hl_node_0 = empty_block(1000000, 1722633600, b"hl-node");
    let block_hl_node_1 = empty_block(1000001, 1722633600, b"hl-node");
    let block_fallback_1 = empty_block(1000001, 1722633600, b"fallback");

    let (temp_dir1, mut file1) = setup_temp_dir_and_file()?;
    writeln!(&mut file1, "{}", serde_json::to_string(&block_hl_node_0)?)?;

    let block_source = HlNodeBlockSource::new(
        BlockSourceBoxed::new(Box::new(block_source_fallback.clone())),
        HlNodeBlockSourceArgs {
            root: temp_dir1.path().to_path_buf(),
            fallback_threshold: DEFAULT_FALLBACK_THRESHOLD_FOR_TEST,
        },
        1000000,
    )
    .await;

    block_source_fallback
        .local_blocks_cache
        .lock()
        .await
        .load_scan_result(scan_result_from_single_block(block_fallback_1.1.clone()));

    Ok(BlockSourceHierarchy {
        block_source,
        _temp_dir: temp_dir1,
        file1,
        current_block: block_hl_node_0,
        future_block_hl_node: block_hl_node_1,
        future_block_fallback: block_fallback_1,
    })
}

#[tokio::test]
async fn test_update_last_fetch_no_fallback() -> eyre::Result<()> {
    let hierarchy = setup_block_source_hierarchy().await?;
    let BlockSourceHierarchy {
        block_source, current_block, future_block_hl_node, mut file1, ..
    } = hierarchy;

    let block = block_source.collect_block(1000000).await.unwrap();
    assert_eq!(block, current_block.1);

    let block = block_source.collect_block(1000001).await;
    assert!(block.is_err());

    writeln!(&mut file1, "{}", serde_json::to_string(&future_block_hl_node)?)?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let block = block_source.collect_block(1000001).await.unwrap();
    assert_eq!(block, future_block_hl_node.1);

    Ok(())
}

#[tokio::test]
async fn test_update_last_fetch_fallback() -> eyre::Result<()> {
    let hierarchy = setup_block_source_hierarchy().await?;
    let BlockSourceHierarchy {
        block_source, current_block, future_block_fallback, mut file1, ..
    } = hierarchy;

    let block = block_source.collect_block(1000000).await.unwrap();
    assert_eq!(block, current_block.1);

    tokio::time::sleep(DEFAULT_FALLBACK_THRESHOLD_FOR_TEST).await;

    writeln!(&mut file1, "{}", serde_json::to_string(&future_block_fallback)?)?;
    let block = block_source.collect_block(1000001).await.unwrap();
    assert_eq!(block, future_block_fallback.1);

    Ok(())
}

#[test]
fn test_hourly_files_sort() -> eyre::Result<()> {
    let temp_dir = tempfile::tempdir()?;
    // create 20250826/9, 20250826/14
    let targets = [("20250826", "9"), ("20250826", "14")];
    for (date, hour) in targets {
        let hourly_file = temp_dir.path().join(HOURLY_SUBDIR).join(date).join(hour);
        let parent = hourly_file.parent().unwrap();
        std::fs::create_dir_all(parent)?;
        std::fs::File::create(hourly_file)?;
    }
    let files = FileOperations::all_hourly_files(temp_dir.path()).unwrap();
    let file_names: Vec<_> =
        files.into_iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();

    assert_eq!(file_names, ["9", "14"]);
    Ok(())
}
