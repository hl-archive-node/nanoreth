use std::sync::Arc;

use clap::Parser;
use reth::{
    builder::{NodeBuilder, NodeHandle, WithLaunchContext},
    rpc::{api::EthPubSubApiServer, eth::RpcNodeCore},
};
use reth_db::DatabaseEnv;
use reth_hl::{
    addons::{
        call_forwarder::{self, CallForwarderApiServer},
        hl_node_compliance::{self, server_restart},
        subscribe_fixup::SubscribeFixup,
        sync_server::{HlSyncApiServer, HlSyncServer, ProviderSyncReader, set_sync_db_reader},
        tx_forwarder::{self, EthForwarderApiServer},
    },
    chainspec::{HlChainSpec, parser::HlChainSpecParser},
    node::{
        HlNode,
        cli::{Cli, HlNodeArgs},
        rpc::precompile::{HlBlockPrecompileApiServer, HlBlockPrecompileExt},
        spot_meta::{self, init as spot_meta_init},
        storage::tables::Tables,
        types::set_spot_metadata_db,
    },
};
use tracing::info;

// We use jemalloc for performance reasons
#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Methods captured from reth's RPC module setup, used to restart the server
/// with the HL compliance HTTP middleware layer.
struct CapturedMethods {
    http: Option<jsonrpsee::Methods>,
    ws: Option<jsonrpsee::Methods>,
    ipc: Option<jsonrpsee::Methods>,
}

fn main() -> eyre::Result<()> {
    reth_cli_util::sigsegv_handler::install();

    // Initialize custom version metadata before parsing CLI so --version uses reth-hl values
    reth_hl::version::init_reth_hl_version();

    Cli::<HlChainSpecParser, HlNodeArgs>::parse().run(
        |builder: WithLaunchContext<NodeBuilder<DatabaseEnv, HlChainSpec>>,
         ext: HlNodeArgs| async move {
            // Apply spot-meta CLI overrides before anything fetches metadata.
            spot_meta::set_spot_meta_url(ext.spot_meta_url.clone());
            let mut spot_meta_overrides = std::collections::BTreeMap::new();
            for entry in &ext.spot_meta_overrides {
                let (address, index) = spot_meta::parse_spot_meta_override(entry)?;
                spot_meta_overrides.insert(address, index);
            }
            spot_meta::add_spot_meta_overrides(spot_meta_overrides);

            let default_upstream_rpc_url = builder.config().chain.official_rpc_url();

            reth_hl::evm::jit::start(ext.jit, ext.jit_hot_threshold, ext.jit_worker_count)?;

            let enable_sync_server = ext.enable_sync_server;
            let hl_node_compliant_default = ext.hl_node_compliant;
            let hl_multiplexed = ext.hl_node_compliant_multiplexed;

            // Shared state to shuttle captured methods between hooks (only used in multiplexed mode)
            let captured: Arc<std::sync::Mutex<CapturedMethods>> =
                Arc::new(std::sync::Mutex::new(CapturedMethods { http: None, ws: None, ipc: None }));
            let captured_for_restart = captured.clone();

            let (node, engine_handle_tx) = HlNode::new(
                ext.block_source_args.parse().await?,
                ext.debug_cutoff_height,
                ext.allow_network_overrides,
            );
            let NodeHandle { node, node_exit_future: exit_future } = builder
                .node(node)
                .extend_rpc_modules(move |mut ctx| {
                    let upstream_rpc_url =
                        ext.upstream_rpc_url.unwrap_or_else(|| default_upstream_rpc_url.to_owned());

                    ctx.modules.replace_configured(
                        tx_forwarder::EthForwarderExt::new(upstream_rpc_url.clone()).into_rpc(),
                    )?;
                    info!("Transaction will be forwarded to {}", upstream_rpc_url);

                    if ext.forward_call {
                        ctx.modules.replace_configured(
                            call_forwarder::CallForwarderExt::new(
                                upstream_rpc_url.clone(),
                                ctx.registry.eth_api().clone(),
                            )
                            .into_rpc(),
                        )?;
                        info!("Call/gas estimation will be forwarded to {}", upstream_rpc_url);
                    }

                    // This is a temporary workaround to fix the issue with custom headers
                    // affects `eth_subscribe[type=newHeads]`
                    ctx.modules.replace_configured(
                        SubscribeFixup::new(
                            Arc::new(ctx.registry.eth_handlers().pubsub.clone()),
                            Arc::new(ctx.registry.eth_api().provider().clone()),
                            ctx.node().task_executor.clone(),
                        )
                        .into_rpc(),
                    )?;

                    if hl_multiplexed {
                        // Multiplexed: install per-request-aware handlers.
                        // --hl-node-compliant controls the default when ?hl= is absent.
                        hl_node_compliance::install(&mut ctx, hl_node_compliant_default)?;
                        info!("hl-node compliant multiplexed mode enabled");
                    } else if hl_node_compliant_default {
                        // Original behavior: unconditional filtering
                        hl_node_compliance::install(&mut ctx, true)?;
                        info!("hl-node compliant mode enabled");
                    }

                    if !ext.experimental_eth_get_proof {
                        ctx.modules.remove_method_from_configured("eth_getProof");
                        info!("eth_getProof is disabled by default");
                    }

                    if enable_sync_server {
                        let provider = ctx.registry.eth_api().provider().clone();
                        set_sync_db_reader(Box::new(ProviderSyncReader::new(provider)));
                        ctx.modules.merge_configured(HlSyncServer.into_rpc())?;
                        info!("Sync server RPC enabled (serving blocks from database)");
                    }

                    ctx.modules.merge_configured(
                        HlBlockPrecompileExt::new(ctx.registry.eth_api().clone()).into_rpc(),
                    )?;

                    // Capture methods for server restart (only in multiplexed mode)
                    if hl_multiplexed {
                        let mut cap = captured.lock().unwrap();
                        cap.http = ctx.modules.http_methods(|_| true);
                        cap.ws = ctx.modules.ws_methods(|_| true);
                        cap.ipc = ctx.modules.ipc_methods(|_| true);
                    }

                    Ok(())
                })
                .on_rpc_started(move |ctx, handles| {
                    if !hl_multiplexed {
                        return Ok(());
                    }

                    let CapturedMethods { http, ws, ipc } = std::mem::replace(
                        &mut *captured_for_restart.lock().unwrap(),
                        CapturedMethods { http: None, ws: None, ipc: None },
                    );

                    if http.is_none() && ws.is_none() && ipc.is_none() {
                        info!("hl-node compliant multiplexed: no RPC methods captured, skipping server restart");
                        return Ok(());
                    }

                    ctx.node().task_executor.clone().spawn_critical_task(
                        "hl-rpc-server-restart",
                        Box::pin(server_restart::restart_servers(
                            handles.rpc,
                            ctx.config().rpc.clone(),
                            http,
                            ws,
                            ipc,
                            hl_node_compliant_default,
                        )),
                    );

                    Ok(())
                })
                .apply(|mut builder| {
                    builder.db_mut().create_and_track_tables_for::<Tables>().expect("create tables");

                    let chain_id = builder.config().chain.inner.chain().id();
                    let db = Arc::new(builder.db_mut().clone());

                    // Set database handle for on-demand persistence
                    set_spot_metadata_db(db.clone());

                    // Load spot metadata from database and initialize cache
                    spot_meta_init::load_spot_metadata_cache(&db, chain_id);

                    builder
                })
                .launch()
                .await?;

            engine_handle_tx.send(node.beacon_engine_handle.clone()).unwrap();

            exit_future.await
        },
    )?;
    Ok(())
}
