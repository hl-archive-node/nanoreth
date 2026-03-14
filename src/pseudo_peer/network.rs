use super::service::{BlockHashCache, BlockPoller};
use crate::{HlPrimitives, chainspec::HlChainSpec, node::network::HlNetworkPrimitives};
use reth_network::{
    NetworkConfig, NetworkManager, PeersConfig,
    config::{SecretKey, rng_secret_key},
};
use reth_provider::test_utils::NoopProvider;
use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
};
use tokio::sync::mpsc;

pub struct NetworkBuilder {
    secret: SecretKey,
    peer_config: PeersConfig,
    discovery_port: u16,
    listener_port: u16,
    chain_spec: HlChainSpec,
    debug_cutoff_height: Option<u64>,
}

impl Default for NetworkBuilder {
    fn default() -> Self {
        Self {
            secret: rng_secret_key(),
            peer_config: PeersConfig::default().with_max_outbound(1).with_max_inbound(0),
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

    pub async fn build<BS>(
        self,
        block_source: Arc<Box<dyn super::sources::BlockSource>>,
        blockhash_cache: BlockHashCache,
    ) -> eyre::Result<(NetworkManager<HlNetworkPrimitives>, mpsc::Sender<()>)> {
        let builder = NetworkConfig::<(), HlNetworkPrimitives>::builder(self.secret)
            .peer_config(self.peer_config)
            .discovery_addr(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), self.discovery_port))
            .listener_addr(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), self.listener_port))
            .disable_discv4_discovery()
            .disable_dns_discovery();
        let chain_id = self.chain_spec.inner.chain().id();

        let (block_poller, start_tx) = BlockPoller::new_suspended(
            chain_id,
            block_source,
            blockhash_cache,
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

pub async fn create_network_manager<BS>(
    chain_spec: HlChainSpec,
    block_source: Arc<Box<dyn super::sources::BlockSource>>,
    blockhash_cache: BlockHashCache,
    debug_cutoff_height: Option<u64>,
) -> eyre::Result<(NetworkManager<HlNetworkPrimitives>, mpsc::Sender<()>)> {
    NetworkBuilder::default()
        .with_chain_spec(chain_spec)
        .with_debug_cutoff_height(debug_cutoff_height)
        .build::<BS>(block_source, blockhash_cache)
        .await
}
