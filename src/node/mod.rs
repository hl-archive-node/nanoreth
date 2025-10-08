use crate::{
    chainspec::HlChainSpec,
    node::{
        pool::HlPoolBuilder,
        primitives::{HlBlock, HlPrimitives},
        rpc::{
            engine_api::{
                builder::HlEngineApiBuilder, payload::HlPayloadTypes,
                validator::HlPayloadValidatorBuilder,
            },
            HlEthApiBuilder,
        },
        storage::HlStorage,
    },
    pseudo_peer::BlockSourceConfig,
};
use consensus::HlConsensusBuilder;
use evm::HlExecutorBuilder;
use network::HlNetworkBuilder;
use reth::{
    api::{FullNodeTypes, NodeTypes},
    builder::{
        components::{ComponentsBuilder, NoopPayloadServiceBuilder},
        rpc::RpcAddOns,
        Node, NodeAdapter,
    },
};
use reth_engine_primitives::ConsensusEngineHandle;
use std::{marker::PhantomData, sync::Arc};
use tokio::sync::{oneshot, Mutex};

pub mod cli;
pub mod consensus;
pub mod engine;
pub mod evm;
pub mod network;
pub mod primitives;
pub mod rpc;
pub mod spot_meta;
pub mod storage;
pub mod types;

/// Hl addons configuring RPC types
pub type HlNodeAddOns<N> =
    RpcAddOns<N, HlEthApiBuilder, HlPayloadValidatorBuilder, HlEngineApiBuilder>;

/// Type configuration for a regular Hl node.
#[derive(Debug, Clone)]
pub struct HlNode {
    engine_handle_rx: Arc<Mutex<Option<oneshot::Receiver<ConsensusEngineHandle<HlPayloadTypes>>>>>,
    block_source_config: BlockSourceConfig,
    debug_cutoff_height: Option<u64>,
}

impl HlNode {
    pub fn new(
        block_source_config: BlockSourceConfig,
        debug_cutoff_height: Option<u64>,
    ) -> (Self, oneshot::Sender<ConsensusEngineHandle<HlPayloadTypes>>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                engine_handle_rx: Arc::new(Mutex::new(Some(rx))),
                block_source_config,
                debug_cutoff_height,
            },
            tx,
        )
    }
}

mod pool;

impl HlNode {
    pub fn components<Node>(
        &self,
    ) -> ComponentsBuilder<
        Node,
        HlPoolBuilder,
        NoopPayloadServiceBuilder,
        HlNetworkBuilder,
        HlExecutorBuilder,
        HlConsensusBuilder,
    >
    where
        Node: FullNodeTypes<Types = Self>,
    {
        ComponentsBuilder::default()
            .node_types::<Node>()
            .pool(HlPoolBuilder)
            .executor(HlExecutorBuilder::default())
            .payload(NoopPayloadServiceBuilder::default())
            .network(HlNetworkBuilder {
                engine_handle_rx: self.engine_handle_rx.clone(),
                block_source_config: self.block_source_config.clone(),
                debug_cutoff_height: self.debug_cutoff_height,
            })
            .consensus(HlConsensusBuilder::default())
    }
}

impl NodeTypes for HlNode {
    type Primitives = HlPrimitives;
    type ChainSpec = HlChainSpec;
    type Storage = HlStorage;
    type Payload = HlPayloadTypes;
}

impl<N> Node<N> for HlNode
where
    N: FullNodeTypes<Types = Self>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        HlPoolBuilder,
        NoopPayloadServiceBuilder,
        HlNetworkBuilder,
        HlExecutorBuilder,
        HlConsensusBuilder,
    >;

    type AddOns = HlNodeAddOns<NodeAdapter<N>>;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        Self::components(self)
    }

    fn add_ons(&self) -> Self::AddOns {
        HlNodeAddOns::new(
            HlEthApiBuilder { _nt: PhantomData },
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
        )
    }
}
