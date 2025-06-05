use std::path::PathBuf;
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
use time::{format_description, OffsetDateTime};
use tracing::{debug, info};

use crate::serialized::{BlockAndReceipts, EvmBlock};
use crate::spot_meta::erc20_contract_to_spot_token;

pub(crate) struct BlockIngest {
    pub ingest_dir: PathBuf,
    pub local_ingest_dir: Option<PathBuf>,
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

pub fn datetime_from_millis(millis: u64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos((millis as i128) * 1_000_000)
        .expect("timestamp out of range")
}

pub fn date_from_datetime(dt: OffsetDateTime) -> String {
    dt.format(&format_description::parse("[year][month][day]").unwrap()).unwrap()
}

impl BlockIngest {
    pub(crate) fn collect_block(&self, height: u64) -> Option<BlockAndReceipts> {
        // self.try_collect_local_block(height).or_else(|| self.try_collect_s3_block(height))
        println!("collectiong s3 block @ {height}");
        self.try_collect_s3_block(height)
    }

    fn fetch_local_blocks_directories(&self) -> Vec<std::path::PathBuf> {
        let mut dirs = Vec::new();
        // Where hourly block data per day is stored, each directory is a day with files for each
        // hour of the day
        let day_data_subdir = self.local_ingest_dir.as_ref().unwrap().join("hourly");

        for entry_result in
            std::fs::read_dir(day_data_subdir).expect("Local blocks directory does not exist")
        {
            let entry = entry_result.unwrap();
            let file_type = entry.file_type().unwrap();

            if file_type.is_dir() {
                dirs.push(entry.path());
            }
        }
        dirs
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
            Some(blocks[0].clone())
        } else {
            None
        }
    }

    pub(crate) fn try_collect_local_block(&self, height: u64) -> Option<BlockAndReceipts> {
        let f = ((height - 1) / 1_000_000) * 1_000_000;
        let s = ((height - 1) / 1_000) * 1_000;
        let path = format!("{}/{f}/{s}/{height}.rmp.lz4", self.ingest_dir.to_string_lossy());
        if std::path::Path::new(&path).exists() {
            let file = std::fs::File::open(path).unwrap();
            let file = std::io::BufReader::new(file);
            let mut decoder = lz4_flex::frame::FrameDecoder::new(file);
            let blocks: Vec<BlockAndReceipts> = rmp_serde::from_read(&mut decoder).unwrap();
            Some(blocks[0].clone())
        } else {
            None
        }
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

        let current_block_timestamp: u64 = provider
            .block_by_number(height - 1)
            .expect("Failed to fetch current block in db")
            .expect("Block does not exist")
            .into_header()
            .timestamp();
        let current_block_timestamp = datetime_from_millis(current_block_timestamp);

        let engine_api = node.auth_server_handle().http_client();
        let mut evm_map = erc20_contract_to_spot_token(node.chain_spec().chain_id()).await?;

        let dirs = self.fetch_local_blocks_directories();
        println!("Current height {height}, timestamp {current_block_timestamp}");

        println!("directories {:?}", dirs);

        panic!("STOP");

        loop {
            let Some(original_block) = self.collect_block(height) else {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                continue;
            };
            let EvmBlock::Reth115(mut block) = original_block.block;
            {
                debug!(target: "reth::cli", ?block, "Built new payload");
                let timestamp = block.header().timestamp();
                println!("new block {height} {timestamp}");

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
