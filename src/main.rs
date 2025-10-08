use std::sync::Arc;

use clap::Parser;
use reth::builder::{NodeBuilder, NodeHandle, WithLaunchContext};
use reth_db::DatabaseEnv;
use reth_hl::{
    addons::{
        call_forwarder::{self, CallForwarderApiServer},
        hl_node_compliance::install_hl_node_compliance,
        tx_forwarder::{self, EthForwarderApiServer},
    },
    chainspec::{parser::HlChainSpecParser, HlChainSpec},
    node::{
        cli::{Cli, HlNodeArgs},
        rpc::precompile::{HlBlockPrecompileApiServer, HlBlockPrecompileExt},
        storage::tables::Tables,
        HlNode,
    },
};
use tracing::info;

// We use jemalloc for performance reasons
#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() -> eyre::Result<()> {
    reth_cli_util::sigsegv_handler::install();

    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }

    // Initialize custom version metadata before parsing CLI so --version uses reth-hl values
    reth_hl::version::init_reth_hl_version();

    Cli::<HlChainSpecParser, HlNodeArgs>::parse().run(
        |builder: WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, HlChainSpec>>,
         ext: HlNodeArgs| async move {
            let default_upstream_rpc_url = builder.config().chain.official_rpc_url();

            let (node, engine_handle_tx) =
                HlNode::new(ext.block_source_args.parse().await?, ext.debug_cutoff_height);
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

                    if ext.hl_node_compliant {
                        install_hl_node_compliance(&mut ctx)?;
                        info!("hl-node compliant mode enabled");
                    }

                    if !ext.experimental_eth_get_proof {
                        ctx.modules.remove_method_from_configured("eth_getProof");
                        info!("eth_getProof is disabled by default");
                    }

                    ctx.modules.merge_configured(
                        HlBlockPrecompileExt::new(ctx.registry.eth_api().clone()).into_rpc(),
                    )?;

                    Ok(())
                })
                .apply(|builder| {
                    builder.db().create_tables_for::<Tables>().expect("create tables");
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
