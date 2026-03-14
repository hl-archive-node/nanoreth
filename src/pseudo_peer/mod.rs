//! A pseudo peer library that ingests multiple block sources to reth
//!
//! This library exposes `start_pseudo_peer` to support reth-side NetworkState/StateFetcher
//! to fetch blocks and feed it to its stages

pub mod cli;
pub mod config;
pub mod network;
pub mod service;
pub mod sources;
pub mod utils;

use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info};

pub use cli::*;
pub use config::*;
pub use network::*;
pub use service::*;
pub use sources::*;

/// Re-export commonly used types
pub mod prelude {
    pub use super::{
        config::BlockSourceConfig,
        service::{BlockPoller, PseudoPeer},
        sources::{
            BlockSource, CachedBlockSource, LocalBlockSource, RpcBlockSource, S3BlockSource,
        },
    };
}

use crate::chainspec::HlChainSpec;
use reth_network::{NetworkEvent, NetworkEventListenerProvider};

/// Main function that starts the network manager and processes eth requests
pub async fn start_pseudo_peer(
    chain_spec: Arc<HlChainSpec>,
    destination_peer: String,
    block_source: BlockSourceBoxed,
    debug_cutoff_height: Option<u64>,
) -> eyre::Result<()> {
    let blockhash_cache = new_blockhash_cache();

    // Create network manager
    let (mut network, start_tx) = create_network_manager::<BlockSourceBoxed>(
        (*chain_spec).clone(),
        destination_peer,
        block_source.clone(),
        blockhash_cache.clone(),
        debug_cutoff_height,
    )
    .await?;

    // Create the channels for receiving eth messages
    let (eth_tx, mut eth_rx) = mpsc::channel(32);
    let (transaction_tx, mut transaction_rx) = mpsc::unbounded_channel();

    network.set_eth_request_handler(eth_tx);
    network.set_transactions(transaction_tx);

    let network_handle = network.handle().clone();
    let mut network_events = network_handle.event_listener();
    info!("Starting network manager...");

    let mut service = PseudoPeer::new(chain_spec, block_source, blockhash_cache.clone());
    tokio::spawn(network);
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
