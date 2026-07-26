use super::{block_store::BlockStore, service::BlockPoller};
use crate::{HlPrimitives, chainspec::HlChainSpec, node::network::HlNetworkPrimitives};
use reth_network::{
    NetworkConfig, NetworkManager, PeersConfig,
    config::{SecretKey, rng_secret_key},
};
use reth::tasks::Runtime;
use reth_provider::test_utils::NoopProvider;
use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
};
use tokio::sync::mpsc;

pub struct NetworkBuilder {
    runtime: Runtime,
    secret: SecretKey,
    peer_config: PeersConfig,
    discovery_port: u16,
    listener_port: u16,
    chain_spec: HlChainSpec,
    debug_cutoff_height: Option<u64>,
}

impl NetworkBuilder {
    pub fn new(runtime: Runtime) -> Self {
        Self {
            runtime,
            secret: rng_secret_key(),
            peer_config: PeersConfig::default().with_max_outbound(1).with_max_inbound(1),
            discovery_port: 0,
            listener_port: 0,
            chain_spec: HlChainSpec::default(),
            debug_cutoff_height: None,
        }
    }
}

impl NetworkBuilder {
    pub fn with_chain_spec(mut self, chain_spec: HlChainSpec) -> Self {
        self.chain_spec = chain_spec;
        self
    }

    pub fn with_debug_cutoff_height(mut self, debug_cutoff_height: Option<u64>) -> Self {
        self.debug_cutoff_height = debug_cutoff_height;
        self
    }

    pub async fn build(
        self,
        block_store: Arc<BlockStore>,
    ) -> eyre::Result<(NetworkManager<HlNetworkPrimitives>, mpsc::Sender<()>)> {
        let builder = NetworkConfig::<(), HlNetworkPrimitives>::builder(self.secret, self.runtime.clone())
            .peer_config(self.peer_config)
            .discovery_addr(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), self.discovery_port))
            .listener_addr(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), self.listener_port))
            .disable_discv4_discovery()
            .disable_dns_discovery();
        let chain_id = self.chain_spec.inner.chain().id();

        let (block_poller, start_tx) = BlockPoller::new_suspended(
            chain_id,
            block_store,
            self.debug_cutoff_height,
        );
        let config = builder.block_import(Box::new(block_poller)).build(Arc::new(NoopProvider::<
            HlChainSpec,
            HlPrimitives,
        >::new(
            self.chain_spec.into(),
        )));

        let network = NetworkManager::new(config).await.map_err(|e| eyre::eyre!(e))?;
        Ok((network, start_tx))
    }
}

pub async fn create_network_manager(
    chain_spec: HlChainSpec,
    block_store: Arc<BlockStore>,
    debug_cutoff_height: Option<u64>,
    runtime: Runtime,
) -> eyre::Result<(NetworkManager<HlNetworkPrimitives>, mpsc::Sender<()>)> {
    NetworkBuilder::new(runtime)
        .with_chain_spec(chain_spec)
        .with_debug_cutoff_height(debug_cutoff_height)
        .build(block_store)
        .await
}
