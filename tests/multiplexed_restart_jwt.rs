//! Regression tests for audit finding `58de7a05`:
//! "Multiplexed RPC restart silently removes configured JWT authentication".
//!
//! `--rpc.jwtsecret` authenticates reth's *regular* HTTP/WS RPC servers (distinct from
//! `--authrpc.jwtsecret`, which guards the engine API). reth plumbs it through
//! `RethRpcServerConfig::rpc_secret_key` → `RpcServerConfig::with_jwt_secret` →
//! `RpcServerConfig::maybe_jwt_layer` → `AuthLayer<JwtAuthValidator>`, on the HTTP, the WS, and
//! the combined same-port server alike.
//!
//! `--hl-node-compliant-multiplexed` stops that server and rebuilds it from the captured method
//! list in `restart_servers`. Before the fix the replacement middleware stack was
//! `HlComplianceLayer` + CORS + optional compression: the `RpcServerArgs` it receives carried
//! `rpc_jwtsecret`, but nothing read it, so every captured method became anonymously reachable.
//! The same reconstruction also fell back to jsonrpsee's default limits instead of the
//! operator-configured ones.
//!
//! Identifying which server answered
//! ---------------------------------
//! The replacement rebinds the same address, so "did the restart happen yet?" needs a beacon.
//! These tests hand `restart_servers` a method set containing an extra [`MARKER`] method that the
//! original server does not serve. A successful `MARKER` call therefore proves the replacement is
//! the one answering — in production the captured set is simply the server's own methods, and the
//! extra method changes nothing about the authentication path under test.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};

use jsonrpsee::{Methods, RpcModule, server::ServerConfigBuilder};
use reth_hl::addons::hl_node_compliance::server_restart::restart_servers;
use reth_node_core::args::RpcServerArgs;
use reth_rpc_builder::{RpcServerConfig, TransportRpcModules};
use reth_rpc_layer::{JwtSecret, secret_to_bearer_header};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

/// Served by the original server and by the replacement.
const PING: &str = "hl_ping";
/// Served *only* by the replacement, so a response to it identifies which server replied.
const MARKER: &str = "hl_restarted";

fn body_for(method: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":[]}}"#)
}

/// Minimal raw HTTP/1.1 POST, so the result depends on nothing but the server under test.
/// Returns `None` if the peer never sent a parseable response.
async fn try_post(addr: SocketAddr, bearer: Option<&str>, body: &str) -> Option<(u16, String)> {
    let mut stream = TcpStream::connect(addr).await.ok()?;

    let auth = bearer.map(|b| format!("Authorization: {b}\r\n")).unwrap_or_default();
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n{auth}Connection: close\r\n\r\n{body}",
        body.len(),
    );
    stream.write_all(request.as_bytes()).await.ok()?;

    // A rejecting middleware answers without draining the request body, so hyper may reset the
    // connection right after the response. Keep whatever arrived rather than failing on that.
    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => break,
            Err(_) => return None,
        }
    }

    let response = String::from_utf8_lossy(&raw).into_owned();
    let status = response.split_whitespace().nth(1)?.parse().ok()?;
    Some((status, response))
}

async fn post(addr: SocketAddr, bearer: Option<&str>, method: &str) -> (u16, String) {
    try_post(addr, bearer, &body_for(method)).await.expect("server answered")
}

/// Starts a JWT-protected RPC server serving [`PING`], exactly as `--rpc.jwtsecret` configures it.
async fn start_protected_server(
    secret: Option<JwtSecret>,
) -> (reth_rpc_builder::RpcServerHandle, SocketAddr) {
    let mut module = RpcModule::new(());
    module.register_method(PING, |_, _, _| "pong").expect("register");

    let handle = RpcServerConfig::default()
        .with_jwt_secret(secret)
        .with_http_address(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .with_http(ServerConfigBuilder::new())
        .start(&TransportRpcModules::default().with_http(module))
        .await
        .expect("start server");

    let addr = handle.http_local_addr().expect("http bound");
    (handle, addr)
}

/// The method set handed to the restart: what the node captured, plus the identifying beacon.
fn captured_methods() -> Methods {
    let mut module = RpcModule::new(());
    module.register_method(PING, |_, _, _| "pong").expect("register");
    module.register_method(MARKER, |_, _, _| "restarted").expect("register");
    module.into()
}

/// Polls until the replacement server is provably the one answering, i.e. until [`MARKER`]
/// resolves. `bearer` must be supplied whenever the restart is expected to preserve auth,
/// otherwise the probe cannot get past the very layer under test.
async fn wait_for_replacement(addr: SocketAddr, bearer: Option<&str>) {
    let body = body_for(MARKER);
    let mut last = None;
    for _ in 0..200 {
        if let Some((status, response)) = try_post(addr, bearer, &body).await {
            if status == 200 && response.contains("restarted") {
                return;
            }
            last = Some((status, response));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("replacement server never served {MARKER} within 10s (last: {last:?})");
}

/// The finding's core claim: after the multiplexed restart, an anonymous caller reached every
/// captured method. The restart must instead carry `--rpc.jwtsecret` over to the new servers.
#[tokio::test(flavor = "multi_thread")]
async fn restart_preserves_configured_rpc_jwt_auth() {
    let secret = JwtSecret::random();
    let (handle, addr) = start_protected_server(Some(secret)).await;

    let bearer = secret_to_bearer_header(&secret);
    let bearer = bearer.to_str().expect("ascii header");

    // Before the restart the JWT layer is doing its job.
    let (status, _) = post(addr, None, PING).await;
    assert_eq!(status, 401, "unauthenticated request must be rejected before the restart");

    let (status, body) = post(addr, Some(bearer), PING).await;
    assert_eq!(status, 200, "authenticated request must succeed before the restart");
    assert!(body.contains("pong"), "server is functional with a valid token: {body}");

    // The restart, exactly as `--hl-node-compliant-multiplexed` performs it.
    let rpc = RpcServerArgs { rpc_jwtsecret: Some(secret), ..Default::default() };
    tokio::spawn(restart_servers(handle, rpc, Some(captured_methods()), None, None, false));

    wait_for_replacement(addr, Some(bearer)).await;

    // The replacement must still reject anonymous callers ...
    let (status, body) = post(addr, None, PING).await;
    assert_eq!(
        status, 401,
        "AUTH BYPASS: the rebuilt server served an unauthenticated request: {body}",
    );
    let (status, body) = post(addr, None, MARKER).await;
    assert_eq!(status, 401, "AUTH BYPASS: an anonymous caller reached a captured method: {body}",);

    // ... while still serving authenticated ones.
    let (status, body) = post(addr, Some(bearer), PING).await;
    assert_eq!(status, 200, "authenticated request must still succeed after the restart");
    assert!(body.contains("pong"), "replacement server is functional: {body}");
}

/// The converse: with no `--rpc.jwtsecret` configured there is nothing to preserve, and the
/// restart must not start rejecting the traffic it is supposed to serve.
#[tokio::test(flavor = "multi_thread")]
async fn restart_without_jwt_secret_stays_open() {
    let (handle, addr) = start_protected_server(None).await;

    let (status, body) = post(addr, None, PING).await;
    assert_eq!(status, 200, "no JWT configured, so anonymous access is expected: {body}");

    tokio::spawn(restart_servers(
        handle,
        RpcServerArgs::default(),
        Some(captured_methods()),
        None,
        None,
        false,
    ));

    wait_for_replacement(addr, None).await;

    let (status, body) = post(addr, None, PING).await;
    assert_eq!(status, 200, "restart must not lock out traffic it should serve: {body}");
    assert!(body.contains("pong"), "replacement server is functional: {body}");
}

/// The same reconstruction dropped the operator's payload limits, falling back to jsonrpsee's
/// 10 MB default in place of reth's 15 MB `--rpc.max-request-size`. A request between the two
/// sizes distinguishes the configurations: it is `413 Payload Too Large` under the default and
/// accepted under the configured limit.
#[tokio::test(flavor = "multi_thread")]
async fn restart_preserves_configured_payload_limits() {
    let (handle, addr) = start_protected_server(None).await;

    tokio::spawn(restart_servers(
        handle,
        RpcServerArgs::default(),
        Some(captured_methods()),
        None,
        None,
        false,
    ));
    wait_for_replacement(addr, None).await;

    // 12 MB: above jsonrpsee's 10 MB default, below reth's 15 MB default.
    let padding = "a".repeat(12 * 1024 * 1024);
    let oversized =
        format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{PING}","params":["{padding}"]}}"#);

    // Under the smaller default the server rejects the body outright — sometimes as
    // `413 Payload Too Large`, sometimes by resetting mid-upload — so a missing response is
    // itself a failure rather than a reason to panic on `expect`.
    let Some((status, body)) = try_post(addr, None, &oversized).await else {
        panic!(
            "no response to a 12 MB request: the restart fell back to jsonrpsee's 10 MB default \
             instead of the configured --rpc.max-request-size",
        );
    };
    assert_ne!(
        status, 413,
        "restart fell back to jsonrpsee's default request limit instead of --rpc.max-request-size",
    );
    assert_eq!(status, 200, "12 MB request is within the configured 15 MB limit: {body}");
}
