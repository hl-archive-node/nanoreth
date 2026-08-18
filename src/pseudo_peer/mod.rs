//! A pseudo peer library that ingests multiple block sources to reth
//!
//! This library exposes `start_pseudo_peer` to support reth-side NetworkState/StateFetcher
//! to fetch blocks and feed it to its stages

pub mod block_store;
pub mod cli;
pub mod config;
pub mod network;
mod patch;
pub mod service;
pub mod sources;
pub mod utils;

use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info};

pub use block_store::*;
pub use cli::*;
pub use config::*;
pub use network::*;
pub use service::*;
pub use sources::*;

/// Re-export commonly used types
pub mod prelude {
    pub use super::{
        block_store::BlockStore,
        config::BlockSourceConfig,
        service::{BlockPoller, PseudoPeer},
        sources::{BlockSource, LocalBlockSource, RpcBlockSource, S3BlockSource},
    };
}

use crate::{chainspec::HlChainSpec, node::network::HlNetworkPrimitives};
use reth_network::{NetworkEvent, NetworkEventListenerProvider, NetworkHandle};
use reth_network_api::{Peers, PeersInfo};

fn configure_pseudo_peer_connection(
    pseudo_peer: &NetworkHandle<HlNetworkPrimitives>,
    destination: &NetworkHandle<HlNetworkPrimitives>,
) {
    // The destination sees this as an inbound connection. Register it up front so adaptive
    // request timeouts cannot lower the pseudo peer's reputation until it is banned.
    let pseudo_peer_record = pseudo_peer.local_node_record();
    destination.add_trusted_peer(pseudo_peer_record.id, pseudo_peer_record.tcp_addr());

    let destination_record = destination.local_node_record();
    pseudo_peer.add_trusted_peer(destination_record.id, destination_record.tcp_addr());
}

/// Main function that starts the network manager and processes eth requests
pub async fn start_pseudo_peer(
    chain_spec: Arc<HlChainSpec>,
    destination_network: NetworkHandle<HlNetworkPrimitives>,
    block_store: Arc<BlockStore>,
    debug_cutoff_height: Option<u64>,
) -> eyre::Result<()> {
    // Create network manager (no boot_nodes — we add the peer directly)
    let (mut network, start_tx) =
        create_network_manager((*chain_spec).clone(), block_store.clone(), debug_cutoff_height)
            .await?;

    // Create the channels for receiving eth messages
    let (eth_tx, mut eth_rx) = mpsc::channel(32);
    let (transaction_tx, mut transaction_rx) = mpsc::unbounded_channel();

    network.set_eth_request_handler(eth_tx);
    network.set_transactions(transaction_tx);

    let network_handle = network.handle().clone();
    let mut network_events = network_handle.event_listener();
    info!("Starting network manager...");

    let mut service = PseudoPeer::new(chain_spec, block_store);
    tokio::spawn(network);

    // Directly add the main node as a peer (bypasses discovery)
    let node_record = destination_network.local_node_record();
    info!(
        peer_id = %node_record.id,
        addr = %node_record.tcp_addr(),
        "Adding main node as direct peer"
    );
    configure_pseudo_peer_connection(&network_handle, &destination_network);

    let mut first = true;

    // Main event loop
    loop {
        tokio::select! {
            Some(event) = tokio_stream::StreamExt::next(&mut network_events) => {
                info!("Network event: {event:?}");
                if matches!(event, NetworkEvent::ActivePeerSession { .. }) && first {
                    start_tx.send(()).await?;
                    first = false;
                }
            }

            _ = transaction_rx.recv() => {}

            Some(eth_req) = eth_rx.recv() => {
                if let Err(e) = service.process_eth_request(eth_req).await {
                    error!("Error processing eth request: {e:?}");
                } else {
                    info!("Processed eth request");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_network_api::{
        PeerKind,
        events::{NetworkPeersEvents, PeerEvent, PeerEventStream},
    };
    use std::{path::Path, time::Duration};
    use tempfile::TempDir;
    use tokio_stream::StreamExt;

    fn local_store(path: &Path) -> Arc<BlockStore> {
        let source: BlockSourceBoxed =
            Arc::new(Box::new(LocalBlockSource::new(path.to_path_buf())));
        Arc::new(BlockStore::new(source, None, 998))
    }

    async fn active_peer_kind(events: &mut PeerEventStream) -> PeerKind {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(PeerEvent::SessionEstablished(info)) = events.next().await {
                    return info.peer_kind;
                }
            }
        })
        .await
        .expect("networks should connect")
    }

    #[tokio::test]
    async fn pseudo_peer_is_trusted_by_main_node() {
        let pseudo_dir = TempDir::new().unwrap();
        let destination_dir = TempDir::new().unwrap();
        let (pseudo_network, _) =
            create_network_manager(HlChainSpec::default(), local_store(pseudo_dir.path()), None)
                .await
                .unwrap();
        let (destination_network, _) = create_network_manager(
            HlChainSpec::default(),
            local_store(destination_dir.path()),
            None,
        )
        .await
        .unwrap();

        let pseudo_handle = pseudo_network.handle().clone();
        let destination_handle = destination_network.handle().clone();
        let mut pseudo_events = pseudo_handle.peer_events();
        let mut destination_events = destination_handle.peer_events();

        configure_pseudo_peer_connection(&pseudo_handle, &destination_handle);
        tokio::spawn(pseudo_network);
        tokio::spawn(destination_network);

        let (pseudo_kind, destination_kind) = tokio::join!(
            active_peer_kind(&mut pseudo_events),
            active_peer_kind(&mut destination_events)
        );
        assert_eq!(pseudo_kind, PeerKind::Trusted);
        assert_eq!(destination_kind, PeerKind::Trusted);
    }
}
