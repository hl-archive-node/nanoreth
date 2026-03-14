#![allow(clippy::owned_cow)]
use crate::{
    HlBlock,
    consensus::HlConsensus,
    node::{
        HlNode,
        network::block_import::{HlBlockImport, handle::ImportHandle, service::ImportService},
        primitives::HlPrimitives,
        rpc::engine_api::payload::HlPayloadTypes,
        types::ReadPrecompileCalls,
    },
    pseudo_peer::{BlockSourceConfig, DbBlockNumberFn, start_pseudo_peer},
};
use alloy_primitives::B256;
use alloy_rlp::{Decodable, Encodable};
use alloy_rpc_types::engine::ForkchoiceState;
use reth::{
    api::{FullNodeTypes, TxTy},
    builder::{BuilderContext, components::NetworkBuilder},
    transaction_pool::{PoolTransaction, TransactionPool},
};
use reth_discv4::NodeRecord;
use reth_engine_primitives::ConsensusEngineHandle;
use reth_eth_wire::{BasicNetworkPrimitives, NewBlock, NewBlockPayload};
use reth_ethereum_primitives::PooledTransactionVariant;
use reth_network::{NetworkConfig, NetworkHandle, NetworkManager};
use reth_network_api::PeersInfo;
use reth_payload_primitives::EngineApiMessageVersion;
use reth_provider::StageCheckpointReader;
use reth_stages_types::StageId;
use reth_storage_api::{BlockHashReader, BlockNumReader};
use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
};
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::info;

pub mod block_import;

/// HL `NewBlock` message value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlNewBlock(pub NewBlock<HlBlock>);

mod rlp {
    use super::*;
    use crate::{
        HlBlockBody, HlHeader,
        node::primitives::{BlockBody, TransactionSigned},
    };
    use alloy_consensus::BlobTransactionSidecar;
    use alloy_primitives::{Address, U128};
    use alloy_rlp::{RlpDecodable, RlpEncodable};
    use alloy_rpc_types::Withdrawals;
    use std::borrow::Cow;

    #[derive(RlpEncodable, RlpDecodable)]
    #[rlp(trailing)]
    struct BlockHelper<'a> {
        header: Cow<'a, HlHeader>,
        transactions: Cow<'a, Vec<TransactionSigned>>,
        ommers: Cow<'a, Vec<HlHeader>>,
        withdrawals: Option<Cow<'a, Withdrawals>>,
    }

    #[derive(RlpEncodable, RlpDecodable)]
    #[rlp(trailing)]
    struct HlNewBlockHelper<'a> {
        block: BlockHelper<'a>,
        td: U128,
        sidecars: Option<Cow<'a, Vec<BlobTransactionSidecar>>>,
        read_precompile_calls: Option<Cow<'a, ReadPrecompileCalls>>,
        highest_precompile_address: Option<Cow<'a, Address>>,
    }

    impl<'a> From<&'a HlNewBlock> for HlNewBlockHelper<'a> {
        fn from(value: &'a HlNewBlock) -> Self {
            let b = &value.0.block;
            Self {
                block: BlockHelper {
                    header: Cow::Borrowed(&b.header),
                    transactions: Cow::Borrowed(&b.body.inner.transactions),
                    ommers: Cow::Borrowed(&b.body.inner.ommers),
                    withdrawals: b.body.inner.withdrawals.as_ref().map(Cow::Borrowed),
                },
                td: value.0.td,
                sidecars: b.body.sidecars.as_ref().map(Cow::Borrowed),
                read_precompile_calls: b.body.read_precompile_calls.as_ref().map(Cow::Borrowed),
                highest_precompile_address: b
                    .body
                    .highest_precompile_address
                    .as_ref()
                    .map(Cow::Borrowed),
            }
        }
    }

    impl Encodable for HlNewBlock {
        fn encode(&self, out: &mut dyn bytes::BufMut) {
            HlNewBlockHelper::from(self).encode(out);
        }

        fn length(&self) -> usize {
            HlNewBlockHelper::from(self).length()
        }
    }

    impl Decodable for HlNewBlock {
        fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
            let h = HlNewBlockHelper::decode(buf)?;
            Ok(HlNewBlock(NewBlock {
                block: HlBlock {
                    header: h.block.header.into_owned(),
                    body: HlBlockBody {
                        inner: BlockBody {
                            transactions: h.block.transactions.into_owned(),
                            ommers: h.block.ommers.into_owned(),
                            withdrawals: h.block.withdrawals.map(|w| w.into_owned()),
                        },
                        sidecars: h.sidecars.map(|s| s.into_owned()),
                        read_precompile_calls: h.read_precompile_calls.map(|s| s.into_owned()),
                        highest_precompile_address: h
                            .highest_precompile_address
                            .map(|s| s.into_owned()),
                    },
                },
                td: h.td,
            }))
        }
    }
}

impl NewBlockPayload for HlNewBlock {
    type Block = HlBlock;

    fn block(&self) -> &Self::Block {
        &self.0.block
    }
}

/// Network primitives for HL.
pub type HlNetworkPrimitives =
    BasicNetworkPrimitives<HlPrimitives, PooledTransactionVariant, HlNewBlock>;

/// A basic hl network builder.
#[derive(Debug)]
pub struct HlNetworkBuilder {
    pub(crate) engine_handle_rx:
        Arc<Mutex<Option<oneshot::Receiver<ConsensusEngineHandle<HlPayloadTypes>>>>>,

    // optional because we might sync from network
    pub(crate) block_source_config: Option<BlockSourceConfig>,

    pub(crate) debug_cutoff_height: Option<u64>,

    pub(crate) allow_network_overrides: bool,
}

impl HlNetworkBuilder {
    /// Returns the [`NetworkConfig`] that contains the settings to launch the p2p network.
    ///
    /// This applies the configured [`HlNetworkBuilder`] settings.
    pub fn network_config<Node>(
        self,
        ctx: &BuilderContext<Node>,
        fcu_trigger_rx: oneshot::Receiver<B256>,
    ) -> eyre::Result<NetworkConfig<Node::Provider, HlNetworkPrimitives>>
    where
        Node: FullNodeTypes<Types = HlNode>,
    {
        let (to_import, from_network) = mpsc::unbounded_channel();
        let (to_network, import_outcome) = mpsc::unbounded_channel();
        let handle = ImportHandle::new(to_import, import_outcome);
        let consensus = Arc::new(HlConsensus { provider: ctx.provider().clone() });

        ctx.task_executor().spawn_critical("block import", async move {
            let engine = self
                .engine_handle_rx
                .lock()
                .await
                .take()
                .expect("node should only be launched once")
                .await
                .unwrap();

            // If a block source provided a target hash, send an initial forkchoice
            // update to the engine to trigger the pipeline. This is needed because
            // the pseudo peer's NewBlock announcements via the network layer don't
            // reliably generate forkchoice updates on this post-merge chain.
            if let Ok(target_hash) = fcu_trigger_rx.await {
                let finalized_hash = consensus
                    .provider
                    .best_block_number()
                    .ok()
                    .and_then(|n| consensus.provider.block_hash(n).ok().flatten())
                    .unwrap_or(target_hash);
                let state = ForkchoiceState {
                    head_block_hash: target_hash,
                    safe_block_hash: finalized_hash,
                    finalized_block_hash: finalized_hash,
                };
                info!(
                    target: "reth::cli",
                    head = %target_hash,
                    finalized = %finalized_hash,
                    "Sending initial forkchoice update to trigger pipeline"
                );
                let _ = engine
                    .fork_choice_updated(state, None, EngineApiMessageVersion::default())
                    .await;
            }

            ImportService::new(consensus, engine, from_network, to_network).await.unwrap();
        });

        let mut config_builder = ctx.network_config_builder()?;

        // Only apply localhost-only network settings if network overrides are NOT allowed
        if !self.allow_network_overrides {
            config_builder = config_builder
                .discovery_addr(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
                .listener_addr(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
                .disable_dns_discovery()
                .disable_nat();
        }

        config_builder = config_builder
            .boot_nodes(boot_nodes())
            .set_head(ctx.head())
            .with_pow()
            .block_import(Box::new(HlBlockImport::new(handle)));

        Ok(ctx.build_network_config(config_builder))
    }
}

impl<Node, Pool> NetworkBuilder<Node, Pool> for HlNetworkBuilder
where
    Node: FullNodeTypes<Types = HlNode>,
    Pool: TransactionPool<
            Transaction: PoolTransaction<
                Consensus = TxTy<Node::Types>,
                Pooled = PooledTransactionVariant,
            >,
        > + Unpin
        + 'static,
{
    type Network = NetworkHandle<HlNetworkPrimitives>;

    async fn build_network(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<Self::Network> {
        let block_source_config = self.block_source_config.clone();
        let debug_cutoff_height = self.debug_cutoff_height;
        let (fcu_trigger_tx, fcu_trigger_rx) = oneshot::channel();
        let handle = ctx.start_network(
            NetworkManager::builder(self.network_config(ctx, fcu_trigger_rx)?).await?,
            pool,
        );
        let local_node_record = handle.local_node_record();
        info!(target: "reth::cli", enode=%local_node_record, "P2P networking initialized");

        if let Some(block_source_config) = block_source_config {
            let next_block_number = ctx
                .provider()
                .get_stage_checkpoint(StageId::Finish)?
                .unwrap_or_default()
                .block_number +
                1;

            // Give the pseudo-peer a handle to the node's database so it can
            // resolve hash→number directly instead of scanning the block source.
            let provider = ctx.provider().clone();
            let db_block_number: DbBlockNumberFn = Arc::new(move |hash| {
                provider.block_number(hash).ok().flatten()
            });

            let chain_spec = ctx.chain_spec();
            let chain_id = chain_spec.inner.chain().id();
            ctx.task_executor().spawn_critical("pseudo peer", async move {
                let block_source = block_source_config
                    .create_cached_block_source((*chain_spec).clone(), next_block_number)
                    .await;

                // Read the latest block and send its hash to trigger the pipeline
                // via a direct forkchoice update. This must happen before
                // start_pseudo_peer (which never returns).
                if let Some(latest) = block_source.find_latest_block_number().await {
                    match block_source.collect_block(latest).await {
                        Ok(block) => {
                            let reth_block = block.to_reth_block(chain_id);
                            let hash = alloy_primitives::Sealable::hash_slow(&reth_block.header);
                            info!(
                                target: "reth::cli",
                                number = %latest,
                                hash = %hash,
                                "Sending forkchoice trigger from block source"
                            );
                            let _ = fcu_trigger_tx.send(hash);
                        }
                        Err(e) => {
                            info!(
                                target: "reth::cli",
                                %e,
                                "Failed to read latest block for forkchoice trigger"
                            );
                        }
                    }
                }

                start_pseudo_peer(
                    chain_spec.clone(),
                    local_node_record.to_string(),
                    block_source,
                    debug_cutoff_height,
                    Some(db_block_number),
                )
                .await
                .unwrap();
            });
        } else {
            info!(target: "reth::cli", "No block source configured - syncing from P2P peers only");
        }

        Ok(handle)
    }
}

/// HL mainnet bootnodes <https://github.com/bnb-chain/hl/blob/master/params/bootnodes.go#L23>
static BOOTNODES: [&str; 0] = [];

pub fn boot_nodes() -> Vec<NodeRecord> {
    BOOTNODES[..].iter().map(|s| s.parse().unwrap()).collect()
}
