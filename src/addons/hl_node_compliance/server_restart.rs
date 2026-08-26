//! Restarts reth's RPC servers with the [`HlComplianceLayer`] HTTP middleware,
//! enabling per-request `?hl=true/false` compliance mode.

use std::net::SocketAddr;

use jsonrpsee::server::ServerConfigBuilder;
use reth_rpc_builder::{RpcServerHandle, config::RethRpcServerConfig};
use reth_rpc_eth_types::EthSubscriptionIdProvider;
use reth_rpc_layer::{AuthLayer, JwtAuthValidator, JwtSecret};

use super::layer::HlComplianceLayer;

/// Builds the [`AuthLayer`] that authenticates the regular RPC transports, mirroring
/// `RpcServerConfig::maybe_jwt_layer`.
///
/// Restarting a server to add middleware must not drop the authentication the operator
/// configured with `--rpc.jwtsecret`; without this the replacement servers would serve every
/// captured method to anonymous callers.
fn maybe_jwt_layer(jwt_secret: Option<JwtSecret>) -> Option<AuthLayer<JwtAuthValidator>> {
    jwt_secret.map(|secret| AuthLayer::new(JwtAuthValidator::new(secret)))
}

/// Stop reth's default RPC servers and restart them with the HL compliance middleware.
pub async fn restart_servers(
    handle: RpcServerHandle,
    rpc: reth_node_core::args::RpcServerArgs,
    http_methods: Option<jsonrpsee::Methods>,
    ws_methods: Option<jsonrpsee::Methods>,
    ipc_methods: Option<jsonrpsee::Methods>,
    hl_default: bool,
) {
    let http_addr = handle.http_local_addr();
    let ws_addr = handle.ws_local_addr();
    let ipc_endpoint = handle.ipc_endpoint();
    let _ = handle.stop();

    let same_port = http_addr.is_some() && ws_addr.is_some() && http_addr == ws_addr;

    // When HTTP+WS share a port, merge methods and start one combined server.
    // Otherwise start each transport separately.
    let servers: Vec<_> = if same_port {
        let addr = http_addr.unwrap();
        let mut module = jsonrpsee::RpcModule::new(());
        if let Some(m) = &http_methods {
            module.merge(m.clone()).expect("no conflicts");
        }
        if let Some(m) = &ws_methods {
            let _ = module.merge(m.clone());
        }
        let cors = rpc
            .http_corsdomain
            .as_deref()
            .or(rpc.ws_allowed_origins.as_deref())
            .and_then(create_cors_layer);
        vec![(addr, module, cors, !rpc.http_disable_compression, None, "HTTP+WS")]
    } else {
        let mut entries = Vec::new();
        if let (Some(addr), Some(methods)) = (http_addr, http_methods) {
            let mut module = jsonrpsee::RpcModule::new(());
            module.merge(methods).expect("no conflicts");
            let cors = rpc.http_corsdomain.as_deref().and_then(create_cors_layer);
            entries.push((addr, module, cors, !rpc.http_disable_compression, Some(true), "HTTP"));
        }
        if let (Some(addr), Some(methods)) = (ws_addr, ws_methods) {
            let mut module = jsonrpsee::RpcModule::new(());
            module.merge(methods).expect("no conflicts");
            let cors = rpc.ws_allowed_origins.as_deref().and_then(create_cors_layer);
            entries.push((addr, module, cors, false, Some(false), "WS"));
        }
        entries
    };

    // Preserve the operator's authentication and connection/payload limits across the restart:
    // the replacement servers must be configured exactly like the ones reth started.
    let jwt_secret = rpc.rpc_secret_key();
    if jwt_secret.is_some() {
        tracing::debug!(target: "reth::cli", "Reapplying --rpc.jwtsecret to the restarted RPC servers");
    }

    let mut http_ws_handles = Vec::new();

    // Restart HTTP/WS servers with compliance middleware
    for (addr, module, cors, compression, http_only, label) in servers {
        let handle = start_http_server(
            addr,
            module,
            hl_default,
            cors,
            compression,
            http_only,
            jwt_secret,
            rpc.http_ws_server_builder(),
        )
        .await;
        tracing::info!(target: "reth::cli", url=%addr, "HL compliance {label} server started");
        http_ws_handles.push(handle);
    }

    // Restart IPC server (no compliance middleware needed — IPC has no HTTP layer).
    // JWT does not apply to IPC (reth does not authenticate it either), but the configured
    // socket permissions and limits must survive the restart.
    let ipc_handle = if let (Some(path), Some(methods)) = (ipc_endpoint, ipc_methods) {
        let mut module = jsonrpsee::RpcModule::new(());
        module.merge(methods).expect("no conflicts");
        let ipc_server = rpc
            .ipc_server_builder()
            .set_id_provider(EthSubscriptionIdProvider::default())
            .build(path.clone());
        let handle = ipc_server.start(module).await.expect("failed to restart IPC server");
        tracing::info!(target: "reth::cli", %path, "IPC server restarted");
        Some(handle)
    } else {
        None
    };

    // Wait for all servers
    for handle in http_ws_handles {
        handle.stopped().await;
    }
    if let Some(handle) = ipc_handle {
        handle.stopped().await;
    }
}

/// Build and start a jsonrpsee HTTP/WS server with the HL compliance middleware layer.
///
/// Retries binding up to 10 times with 50ms intervals to handle port release after server stop.
#[allow(clippy::too_many_arguments)]
async fn start_http_server(
    addr: SocketAddr,
    module: jsonrpsee::RpcModule<()>,
    hl_default: bool,
    cors: Option<tower_http::cors::CorsLayer>,
    compression: bool,
    http_only: Option<bool>,
    jwt_secret: Option<JwtSecret>,
    server_config: ServerConfigBuilder,
) -> jsonrpsee::server::ServerHandle {
    use jsonrpsee::server::Server;

    let mut last_err = None;
    for attempt in 0..10 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        // Layer order mirrors reth: CORS outermost so preflight `OPTIONS` requests — which
        // carry no `Authorization` header — are answered before authentication runs.
        let mut builder = Server::builder().set_http_middleware(
            tower::ServiceBuilder::new()
                .option_layer(cors.clone())
                .option_layer(maybe_jwt_layer(jwt_secret))
                .layer(HlComplianceLayer::new(hl_default))
                .option_layer(if compression {
                    Some(reth_rpc_layer::CompressionLayer::new())
                } else {
                    None
                }),
        );

        let mut config =
            server_config.clone().set_id_provider(EthSubscriptionIdProvider::default());
        config = match http_only {
            Some(true) => config.http_only(),
            Some(false) => config.ws_only(),
            None => config,
        };
        builder = builder.set_config(config.build());

        match builder.build(addr).await {
            Ok(server) => return server.start(module),
            Err(e) => last_err = Some(e),
        }
    }
    panic!("failed to restart RPC server on {addr} after 10 attempts: {}", last_err.unwrap());
}

fn create_cors_layer(cors_domain: &str) -> Option<tower_http::cors::CorsLayer> {
    use tower_http::cors::{AllowOrigin, CorsLayer};

    let trimmed = cors_domain.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "*" {
        return Some(CorsLayer::permissive());
    }
    let origins: Vec<http::HeaderValue> = trimmed
        .split(',')
        .filter_map(|s| {
            let parsed = s.trim().parse();
            if parsed.is_err() {
                tracing::warn!(target: "reth::cli", origin = s.trim(), "Skipping unparseable CORS origin");
            }
            parsed.ok()
        })
        .collect();
    if origins.is_empty() {
        None
    } else {
        Some(
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(origins))
                .allow_methods([http::Method::GET, http::Method::POST])
                .allow_headers(tower_http::cors::Any),
        )
    }
}
