use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alloy_consensus::{BlockBody, BlockHeader, Transaction};
use alloy_primitives::TxKind;
use alloy_primitives::{Address, PrimitiveSignature, B256, U256};
use alloy_rpc_types::engine::{
    ExecutionPayloadEnvelopeV3, ForkchoiceState, PayloadAttributes, PayloadStatusEnum,
};
use jsonrpsee::http_client::{transport::HttpBackend, HttpClient};
use reth::network::PeersHandleProvider;
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_hyperliquid_types::PrecompilesCache;
use reth_node_api::{Block, FullNodeComponents, PayloadTypes};
use reth_node_builder::EngineTypes;
use reth_node_builder::NodeTypesWithEngine;
use reth_node_builder::{rpc::RethRpcAddOns, FullNode};
use reth_payload_builder::{EthBuiltPayload, EthPayloadBuilderAttributes, PayloadId};
use reth_primitives::{Transaction as TypedTransaction, TransactionSigned};
use reth_provider::{BlockHashReader, BlockReader, StageCheckpointReader};
use reth_rpc_api::EngineApiClient;
use reth_rpc_layer::AuthClientService;
use reth_stages::StageId;
use serde::Deserialize;
use time::{format_description, Duration, OffsetDateTime};
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::serialized::{BlockAndReceipts, EvmBlock};
use crate::spot_meta::erc20_contract_to_spot_token;

/// Poll interval when tailing an *open* hourly file.
const TAIL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);
/// Sub‑directory that contains day folders (inside `local_ingest_dir`).
const HOURLY_SUBDIR: &str = "hourly";

pub(crate) struct BlockIngest {
    pub ingest_dir: PathBuf,
    pub local_ingest_dir: Option<PathBuf>,
    pub local_blocks_cache: Arc<Mutex<BTreeMap<u64, BlockAndReceipts>>>, // height → block
    pub precompiles_cache: PrecompilesCache,
}

#[derive(Deserialize)]
struct LocalBlockAndReceipts(String, BlockAndReceipts);

struct ScanResult {
    next_expected_height: u64,
    new_blocks: Vec<BlockAndReceipts>,
}

fn scan_hour_file(path: &Path, last_line: &mut usize, start_height: u64) -> ScanResult {
    // info!(
    //     "Scanning hour block file @ {:?} for height [{:?}] | Last Line {:?}",
    //     path, start_height, last_line
    // );
    let file = std::fs::File::open(path).expect("Failed to open hour file path");
    let reader = BufReader::new(file);

    let mut new_blocks = Vec::<BlockAndReceipts>::new();
    let mut last_height = start_height;
    let lines: Vec<String> = reader.lines().collect::<Result<_, _>>().unwrap();
    let skip = if *last_line == 0 { 0 } else { (last_line.clone()) - 1 };

    for (line_idx, line) in lines.iter().enumerate().skip(skip) {
        // Safety check ensuring efficiency
        if line_idx < *last_line {
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }

        let LocalBlockAndReceipts(_block_timestamp, parsed_block): LocalBlockAndReceipts =
            serde_json::from_str(&line).expect("Failed to parse local block and receipts");

        let height = match &parsed_block.block {
            EvmBlock::Reth115(b) => {
                let block_number = b.header().number() as u64;
                // Another check to ensure not returning an older block
                if block_number < start_height {
                    continue;
                }
                block_number
            }
        };
        // println!("Iterating block height {:?} | Line {}", height, line_idx);
        if height >= start_height {
            last_height = last_height.max(height);
            new_blocks.push(parsed_block);
            *last_line = line_idx;
        }
    }

    ScanResult { next_expected_height: last_height + 1, new_blocks }
}

async fn submit_payload<Engine: PayloadTypes + EngineTypes>(
    engine_api_client: &HttpClient<AuthClientService<HttpBackend>>,
    payload: EthBuiltPayload,
    payload_builder_attributes: EthPayloadBuilderAttributes,
    expected_status: PayloadStatusEnum,
) -> Result<B256, Box<dyn std::error::Error>> {
    let versioned_hashes =
        payload.block().blob_versioned_hashes_iter().copied().collect::<Vec<_>>();
    // submit payload to engine api
    let submission = {
        let envelope: ExecutionPayloadEnvelopeV3 =
            <EthBuiltPayload as Into<ExecutionPayloadEnvelopeV3>>::into(payload);
        EngineApiClient::<Engine>::new_payload_v3(
            engine_api_client,
            envelope.execution_payload,
            versioned_hashes,
            payload_builder_attributes.parent_beacon_block_root.unwrap(),
        )
        .await?
    };

    assert_eq!(submission.status.as_str(), expected_status.as_str());

    Ok(submission.latest_valid_hash.unwrap_or_default())
}

fn datetime_from_timestamp(ts_sec: u64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos((ts_sec as i128) * 1_000 * 1_000_000)
        .expect("timestamp out of range")
}

fn date_from_datetime(dt: OffsetDateTime) -> String {
    dt.format(&format_description::parse("[year][month][day]").unwrap()).unwrap()
}

impl BlockIngest {
    pub(crate) async fn collect_block(&self, height: u64) -> Option<BlockAndReceipts> {
        // info!("Attempting to collect block @ height [{height}]");

        // Not a one liner (using .or) to include logs
        if let Some(block) = self.try_collect_local_block(height).await {
            info!("Returning locally synced block for @ Height [{height}]");
            return Some(block);
        } else {
            self.try_collect_s3_block(height)
        }
    }

    pub(crate) fn try_collect_s3_block(&self, height: u64) -> Option<BlockAndReceipts> {
        let f = ((height - 1) / 1_000_000) * 1_000_000;
        let s = ((height - 1) / 1_000) * 1_000;
        let path = format!("{}/{f}/{s}/{height}.rmp.lz4", self.ingest_dir.to_string_lossy());
        if std::path::Path::new(&path).exists() {
            let file = std::fs::File::open(path).unwrap();
            let file = std::io::BufReader::new(file);
            let mut decoder = lz4_flex::frame::FrameDecoder::new(file);
            let blocks: Vec<BlockAndReceipts> = rmp_serde::from_read(&mut decoder).unwrap();
            info!("Returning s3 synced block for @ Height [{height}]");
            Some(blocks[0].clone())
        } else {
            None
        }
    }

    async fn try_collect_local_block(&self, height: u64) -> Option<BlockAndReceipts> {
        let mut u_cache = self.local_blocks_cache.lock().await;
        u_cache.remove(&height)
    }

    async fn start_local_ingest_loop(&self, current_head: u64, current_ts: u64) {
        let Some(root) = &self.local_ingest_dir else { return }; // nothing to do
        let root = root.to_owned();
        let cache = self.local_blocks_cache.clone();
        let precompiles_cache = self.precompiles_cache.clone();

        tokio::spawn(async move {
            let mut next_height = current_head;
            let mut dt = datetime_from_timestamp(current_ts)
                .replace_minute(0)
                .unwrap()
                .replace_second(0)
                .unwrap()
                .replace_nanosecond(0)
                .unwrap();

            let mut hour = dt.hour();
            let mut day_str = date_from_datetime(dt);
            let mut last_line = 0;

            loop {
                let hour_file = root.join(HOURLY_SUBDIR).join(&day_str).join(format!("{hour}"));

                if hour_file.exists() {
                    let ScanResult { next_expected_height, new_blocks } =
                        scan_hour_file(&hour_file, &mut last_line, next_height);
                    if !new_blocks.is_empty() {
                        let mut u_cache = cache.lock().await;
                        let mut u_pre_cache = precompiles_cache.lock();
                        for blk in new_blocks {
                            let precompiles = blk.read_precompile_calls.clone();
                            let h = match &blk.block {
                                EvmBlock::Reth115(b) => {
                                    let block_number = b.header().number() as u64;
                                    block_number
                                }
                            };
                            u_cache.insert(h, blk);
                            u_pre_cache.insert(h, precompiles);
                        }
                        next_height = next_expected_height;
                    }
                }

                // Decide whether the *current* hour file is closed (past) or
                // still live. If it’s in the past by > 1 h, move to next hour;
                // otherwise, keep tailing the same file.
                let now = OffsetDateTime::now_utc();

                // println!("Date Current {:?}", dt);
                // println!("Now Current {:?}", now);

                if dt + Duration::HOUR < now {
                    dt += Duration::HOUR;
                    hour = dt.hour();
                    day_str = date_from_datetime(dt);
                    last_line = 0;
                    info!(
                        "Moving to a new file. {:?}",
                        root.join(HOURLY_SUBDIR).join(&day_str).join(format!("{hour}"))
                    );
                    continue;
                }

                tokio::time::sleep(TAIL_INTERVAL).await;
            }
        });
    }

    pub(crate) async fn run<Node, Engine, AddOns>(
        &self,
        node: FullNode<Node, AddOns>,
    ) -> Result<(), Box<dyn std::error::Error>>
    where
        Node: FullNodeComponents,
        AddOns: RethRpcAddOns<Node>,
        Engine: EngineTypes,
        Node::Types: NodeTypesWithEngine<ChainSpec: EthereumHardforks, Engine = Engine>,
        Node::Network: PeersHandleProvider,
        AddOns: RethRpcAddOns<Node>,
        Engine::ExecutionPayloadEnvelopeV3: From<Engine::BuiltPayload>,
        Engine::ExecutionPayloadEnvelopeV4: From<Engine::BuiltPayload>,
    {
        let provider = &node.provider;
        let checkpoint = provider.get_stage_checkpoint(StageId::Finish)?;
        let head = checkpoint.unwrap_or_default().block_number;
        let genesis_hash = node.chain_spec().genesis_hash();

        let mut height = head + 1;
        let mut previous_hash = provider.block_hash(head)?.unwrap_or(genesis_hash);
        let mut previous_timestamp =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();

        let engine_api = node.auth_server_handle().http_client();
        let mut evm_map = erc20_contract_to_spot_token(node.chain_spec().chain_id()).await?;

        let current_block_timestamp: u64 = provider
            .block_by_number(head)
            .expect("Failed to fetch current block in db")
            .expect("Block does not exist")
            .into_header()
            .timestamp();

        info!("Current height {height}, timestamp {current_block_timestamp}");
        self.start_local_ingest_loop(height, current_block_timestamp).await;

        loop {
            let Some(original_block) = self.collect_block(height).await else {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                continue;
            };
            let EvmBlock::Reth115(mut block) = original_block.block;
            {
                debug!(target: "reth::cli", ?block, "Built new payload");
                let timestamp = block.header().timestamp();

                let block_hash = block.clone().try_recover()?.hash();
                {
                    let BlockBody { transactions, ommers, withdrawals } =
                        std::mem::take(block.body_mut());
                    let mut system_txs = vec![];

                    for transaction in original_block.system_txs {
                        let TypedTransaction::Legacy(tx) = &transaction.tx else {
                            panic!("Unexpected transaction type");
                        };
                        let TxKind::Call(to) = tx.to else {
                            panic!("Unexpected contract creation");
                        };
                        let s = if tx.input().is_empty() {
                            U256::from(0x1)
                        } else {
                            loop {
                                if let Some(spot) = evm_map.get(&to) {
                                    break spot.to_s();
                                }

                                info!(
                                    "Contract not found: {:?} from spot mapping, fetching again...",
                                    to
                                );
                                evm_map =
                                    erc20_contract_to_spot_token(node.chain_spec().chain_id())
                                        .await?;
                            }
                        };
                        let signature = PrimitiveSignature::new(
                            // from anvil
                            U256::from(0x1),
                            s,
                            true,
                        );
                        let typed_transaction = transaction.tx;
                        let tx = TransactionSigned::new(
                            typed_transaction,
                            signature,
                            Default::default(),
                        );
                        system_txs.push(tx);
                    }
                    let mut txs = vec![];
                    txs.extend(system_txs);
                    txs.extend(transactions);
                    *block.body_mut() = BlockBody { transactions: txs, ommers, withdrawals };
                }

                let total_fees = U256::ZERO;
                let payload = EthBuiltPayload::new(
                    PayloadId::new(height.to_be_bytes()),
                    Arc::new(block),
                    total_fees,
                    None,
                );

                let attributes = EthPayloadBuilderAttributes::new(
                    B256::ZERO,
                    PayloadAttributes {
                        timestamp,
                        prev_randao: B256::ZERO,
                        suggested_fee_recipient: Address::ZERO,
                        withdrawals: Some(vec![]),
                        parent_beacon_block_root: Some(B256::ZERO),
                    },
                );
                submit_payload::<Engine>(
                    &engine_api,
                    payload,
                    attributes,
                    PayloadStatusEnum::Valid,
                )
                .await?;

                let current_timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis();

                if height % 100 == 0 || current_timestamp - previous_timestamp > 100 {
                    EngineApiClient::<Engine>::fork_choice_updated_v2(
                        &engine_api,
                        ForkchoiceState {
                            head_block_hash: block_hash,
                            safe_block_hash: previous_hash,
                            finalized_block_hash: previous_hash,
                        },
                        None,
                    )
                    .await
                    .unwrap();
                    previous_timestamp = current_timestamp;
                }
                previous_hash = block_hash;
            }
            height += 1;
        }
    }
}
