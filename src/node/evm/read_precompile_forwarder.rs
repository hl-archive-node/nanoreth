//! Serves read precompile calls at the chain head by forwarding them to an hl-node.
//!
//! Every block body carries the read precompile calls that its own transactions made
//! (`ReadPrecompileCalls`), which is all that re-executing a historical block needs. It is not
//! enough for `eth_call` / `eth_estimateGas` / `debug_traceCall` against the chain head: those
//! run inputs that no transaction has made yet, and nanoreth holds no HyperCore state of its own
//! to answer them with, so today they fail with out-of-gas. This module forwards such inputs to a
//! node that does hold that state — typically a local hl-node — and prices the response locally.
//!
//! See <https://github.com/hl-archive-node/nanoreth/issues/26>.

use alloy_primitives::{Address, B256, Bytes};
use jsonrpsee::{
    http_client::{HttpClient, HttpClientBuilder},
    rpc_params,
    types::ErrorObject,
};
use jsonrpsee_core::{ClientError, client::ClientT};
use parking_lot::{Condvar, Mutex};
use revm::precompile::{PrecompileError, PrecompileOutput, PrecompileResult};
use std::{
    collections::HashMap,
    num::NonZeroU32,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use tokio::runtime::Handle;
use tracing::{debug, warn};

/// Gas an HL read precompile charges regardless of payload size.
pub const READ_PRECOMPILE_BASE_GAS: u64 = 1000;

/// Gas an HL read precompile charges per byte of input plus output.
pub const READ_PRECOMPILE_GAS_PER_BYTE: u64 = 33;

/// Gas charged by a read precompile for the given input and output sizes.
///
/// hl-node does not report the gas a read precompile used over `eth_call`, but it is a pure
/// function of the payload sizes, so the forwarder only has to fetch the output bytes. The
/// constants were measured by binary searching the minimum gas limit that lets a direct call to
/// each precompile succeed on mainnet; see the tests below for the samples they were fit to.
pub const fn read_precompile_gas(input_len: usize, output_len: usize) -> u64 {
    READ_PRECOMPILE_BASE_GAS + READ_PRECOMPILE_GAS_PER_BYTE * (input_len as u64 + output_len as u64)
}

/// Timeout for a single forwarded call.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on cached responses, so that a long-lived head cannot grow the cache without
/// bound.
const MAX_CACHE_ENTRIES: usize = 16_384;

/// What the upstream said about one `(address, input)` pair.
#[derive(Clone, Debug)]
enum Response {
    /// The precompile returned these bytes.
    Output(Bytes),
    /// The precompile rejected the input.
    Rejected,
}

/// Responses, keyed by the canonical head they were fetched for.
///
/// The block number and hash are part of the key rather than a scope for the whole map, because
/// calls at the head and calls at the pending block are in flight at the same time — `eth_call`
/// against `latest` sits at the head while `eth_simulateV1` builds on top of it. The hash keeps a
/// same-height reorg from reusing responses fetched for the replaced head.
#[derive(Debug, Default)]
struct Cache {
    /// Highest block seen, so entries the chain has moved past can be dropped first.
    newest_block_number: u64,
    responses: HashMap<(u64, B256, Address, Bytes), Response>,
    in_flight: HashMap<(u64, B256, Address, Bytes), Arc<InFlight>>,
}

#[derive(Debug, Default)]
struct InFlight {
    result: Mutex<Option<Result<Response, String>>>,
    ready: Condvar,
}

/// Resolves read precompile calls that the chain head did not record by asking an upstream
/// hl-node for them.
pub struct ReadPrecompileForwarder {
    client: HttpClient,
    handle: Handle,
    rate_limiter: Option<Arc<RequestRateLimiter>>,
    /// Responses already fetched from the upstream.
    ///
    /// Repeated calls are the norm rather than the exception: `eth_estimateGas` binary searches
    /// over the same transaction, re-running every precompile call on each iteration. Caching
    /// also makes all reads within one RPC request agree with each other, which separate
    /// upstream calls do not guarantee.
    cache: Mutex<Cache>,
}

#[derive(Debug)]
struct RequestRateLimiter {
    period: Duration,
    next_request: Mutex<Instant>,
}

impl RequestRateLimiter {
    fn new(requests_per_second: NonZeroU32) -> Self {
        let period = Duration::from_secs_f64(1.0 / f64::from(requests_per_second.get()));
        Self { period, next_request: Mutex::new(Instant::now()) }
    }

    fn wait(&self) {
        let now = Instant::now();
        let scheduled = {
            let mut next_request = self.next_request.lock();
            let scheduled = (*next_request).max(now);
            *next_request = scheduled + self.period;
            scheduled
        };
        std::thread::sleep(scheduled.saturating_duration_since(now));
    }
}

/// Summarises the cache rather than dumping it.
///
/// The execution context holds one of these and derives [`Debug`], so a derived impl here would
/// print every cached response — up to [`MAX_CACHE_ENTRIES`] of them — into any log line or error
/// that formats the context.
impl std::fmt::Debug for ReadPrecompileForwarder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cache = self.cache.lock();
        f.debug_struct("ReadPrecompileForwarder")
            .field("newest_block_number", &cache.newest_block_number)
            .field("cached_responses", &cache.responses.len())
            .field("in_flight_requests", &cache.in_flight.len())
            .field("rate_limited", &self.rate_limiter.is_some())
            .finish_non_exhaustive()
    }
}

impl ReadPrecompileForwarder {
    /// Creates a forwarder that resolves calls against `url`.
    ///
    /// `handle` drives the requests, since precompiles are invoked from blocking RPC workers.
    pub fn new(
        url: &str,
        handle: Handle,
        requests_per_second: Option<NonZeroU32>,
    ) -> eyre::Result<Self> {
        let client = HttpClientBuilder::default().request_timeout(REQUEST_TIMEOUT).build(url)?;
        let rate_limiter = requests_per_second.map(|rate| Arc::new(RequestRateLimiter::new(rate)));
        Ok(Self { client, handle, rate_limiter, cache: Mutex::new(Cache::default()) })
    }

    /// Runs `input` against the read precompile at `address` as of the chain head.
    ///
    /// `block_number` only scopes the cache. The upstream is always asked for `latest`, and there
    /// is no point asking it for anything else: hl-node overrides the requested block with the
    /// head, since HyperCore state is not archived. That is also why the forwarder is installed
    /// for the head alone — a historical read precompile call can only be answered by replaying
    /// the block's recorded calls.
    pub fn call(
        &self,
        address: Address,
        input: &[u8],
        gas_limit: u64,
        block_number: u64,
        head_hash: B256,
    ) -> PrecompileResult {
        let input = Bytes::copy_from_slice(input);
        match self.resolve(address, &input, block_number, head_hash)? {
            Response::Output(bytes) => {
                let gas_used = read_precompile_gas(input.len(), bytes.len());
                if gas_used > gas_limit {
                    return Err(PrecompileError::OutOfGas);
                }
                Ok(PrecompileOutput { gas_used, bytes, reverted: false })
            }
            // HL burns the whole gas limit when a read precompile rejects its input, which is
            // also how `ReadPrecompileResult::Error` is replayed for recorded blocks.
            Response::Rejected => Err(PrecompileError::OutOfGas),
        }
    }

    fn resolve(
        &self,
        address: Address,
        input: &Bytes,
        block_number: u64,
        head_hash: B256,
    ) -> Result<Response, PrecompileError> {
        let key = (block_number, head_hash, address, input.clone());

        let (in_flight, leader) = {
            let mut cache = self.cache.lock();
            if let Some(cached) = cache.responses.get(&key) {
                return Ok(cached.clone());
            }
            if let Some(in_flight) = cache.in_flight.get(&key) {
                (in_flight.clone(), false)
            } else {
                let in_flight = Arc::new(InFlight::default());
                cache.in_flight.insert(key.clone(), in_flight.clone());
                (in_flight, true)
            }
        };

        if !leader {
            let mut result = in_flight.result.lock();
            while result.is_none() {
                in_flight.ready.wait(&mut result);
            }
            return result.clone().unwrap().map_err(PrecompileError::Fatal);
        }

        let result = self.request(address, input).map_err(|err| err.to_string());
        let mut cache = self.cache.lock();
        cache.in_flight.remove(&key);
        if let Ok(response) = &result {
            cache.newest_block_number = cache.newest_block_number.max(block_number);
            if cache.responses.len() >= MAX_CACHE_ENTRIES {
                // Drop what the chain has moved past first, so a burst of calls at the head does not
                // throw away the entries it is still working through.
                let newest = cache.newest_block_number;
                cache.responses.retain(|(block, _, _, _), _| *block >= newest);
                if cache.responses.len() >= MAX_CACHE_ENTRIES {
                    cache.responses.clear();
                }
            }
            cache.responses.insert(key, response.clone());
        }
        drop(cache);

        *in_flight.result.lock() = Some(result.clone());
        in_flight.ready.notify_all();
        result.map_err(PrecompileError::Fatal)
    }

    fn request(&self, address: Address, input: &Bytes) -> Result<Response, PrecompileError> {
        if let Some(rate_limiter) = &self.rate_limiter {
            rate_limiter.wait();
        }
        let client = self.client.clone();
        let params = rpc_params![serde_json::json!({ "to": address, "data": input }), "latest"];

        // Precompiles are called from blocking RPC workers, so the request is driven on the
        // node's runtime and waited on here rather than awaited.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.handle.spawn(async move {
            let _ = tx.send(client.request::<Bytes, _>("eth_call", params).await);
        });

        // The client times out on its own; this only guards against the response never arriving.
        let result = rx.recv_timeout(REQUEST_TIMEOUT * 2).map_err(|_| {
            warn!(target: "rpc::eth", %address, "Timed out forwarding read precompile call");
            PrecompileError::Fatal(format!("read precompile {address} timed out"))
        })?;

        match result {
            Ok(bytes) => Ok(Response::Output(bytes)),
            Err(ClientError::Call(err)) if is_rejected_input(&err) => {
                debug!(target: "rpc::eth", %address, message = err.message(), "Read precompile rejected input");
                Ok(Response::Rejected)
            }
            Err(err) => {
                warn!(target: "rpc::eth", %address, %err, "Failed to forward read precompile call");
                Err(PrecompileError::Fatal(format!(
                    "failed to forward read precompile {address}: {err}"
                )))
            }
        }
    }
}

/// JSON-RPC code hl-node answers a failed `eth_call` with.
const EXECUTION_ERROR_CODE: i32 = -32003;

/// Whether a JSON-RPC error means the upstream ran the precompile and it refused the input,
/// rather than that the upstream could not serve the request at all.
///
/// Only the former burns the gas limit. Everything else — a wrong URL, a node without `eth_call`,
/// a rate limit, an overloaded upstream — has to surface as an RPC error, because reporting it as
/// a refused input would hand the caller a plausible but wrong out-of-gas result instead of a
/// visible failure.
///
/// This deliberately matches on the message as well as the code. The codes EVM RPCs use for
/// execution failures (`-32003`, and `-32000` in particular) double as generic server-error
/// catch-alls, so matching the code alone is what lets a throttled response through as a refused
/// input. Being too strict only costs a loud error where a quiet one would do.
fn is_rejected_input(err: &ErrorObject<'_>) -> bool {
    err.code() == EXECUTION_ERROR_CODE && err.message().contains("PrecompileError")
}

static FORWARDER: OnceLock<Arc<ReadPrecompileForwarder>> = OnceLock::new();

/// Installs the process-wide forwarder used by the RPC call paths.
pub fn set_read_precompile_forwarder(forwarder: ReadPrecompileForwarder) {
    if FORWARDER.set(Arc::new(forwarder)).is_err() {
        warn!(target: "rpc::eth", "Read precompile forwarder is already installed");
    }
}

/// The installed forwarder, if `--forward-read-precompiles` was set.
pub fn read_precompile_forwarder() -> Option<Arc<ReadPrecompileForwarder>> {
    FORWARDER.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(input_len, output_len, gas)` samples taken from mainnet by binary searching the minimum
    /// gas limit that lets a direct `eth_call` to a read precompile succeed, minus the intrinsic
    /// cost of the transaction. Covers `0x801`, `0x806`..`0x80a`, `0x80c` and `0x80e`.
    const SAMPLES: [(usize, usize, u64); 6] = [
        (0, 32, 2056),
        (32, 32, 3112),
        (32, 64, 4168),
        (64, 96, 6280),
        (32, 256, 10504),
        (32, 384, 14728),
    ];

    #[test]
    fn gas_matches_measured_samples() {
        for (input_len, output_len, expected) in SAMPLES {
            assert_eq!(
                read_precompile_gas(input_len, output_len),
                expected,
                "input_len={input_len} output_len={output_len}"
            );
        }
    }

    #[test]
    fn only_a_refused_input_burns_gas() {
        let err = |code, message: &str| ErrorObject::owned(code, message.to_string(), None::<()>);

        // What hl-node answers a read precompile that refused its input with.
        assert!(is_rejected_input(&err(EXECUTION_ERROR_CODE, "EVM error: PrecompileError")));

        // Everything else has to surface as an error rather than as an out-of-gas result: an
        // upstream that throttled or fell over never ran the precompile at all.
        for (code, message) in [
            (EXECUTION_ERROR_CODE, "rate limit exceeded"),
            (-32000, "rate limited"),
            (-32000, "execution timeout"),
            (-32601, "method eth_call is not available"),
            (-32603, "internal error"),
            (
                EXECUTION_ERROR_CODE,
                "out of gas: gas exhausted during precompiled contract execution: 21000",
            ),
        ] {
            assert!(!is_rejected_input(&err(code, message)), "{code} {message}");
        }
    }

    #[test]
    fn same_height_reorg_has_a_distinct_cache_key() {
        let address = Address::repeat_byte(1);
        let input = Bytes::from_static(&[2]);
        let old_head = B256::repeat_byte(3);
        let new_head = B256::repeat_byte(4);
        let mut cache = Cache::default();

        cache.responses.insert(
            (10, old_head, address, input.clone()),
            Response::Output(Bytes::from_static(&[5])),
        );

        assert!(!cache.responses.contains_key(&(10, new_head, address, input)));
    }

    /// `l1BlockNumber`, which takes no input and returns one word.
    const L1_BLOCK_NUMBER: Address =
        alloy_primitives::address!("0x0000000000000000000000000000000000000809");

    /// `bbo`, the precompile whose unrecorded call made the `eth_call` in
    /// <https://github.com/hl-archive-node/nanoreth/issues/142> revert, and the input it was
    /// reached with there.
    const BBO: Address = alloy_primitives::address!("0x000000000000000000000000000000000000080e");
    const BBO_INPUT: [u8; 32] = alloy_primitives::hex!(
        "0x000000000000000000000000000000000000000000000000000000000000277b"
    );

    /// Checks the whole round trip — request shape, error mapping and pricing — against a real
    /// hl-node. Ignored by default because it needs network access.
    ///
    /// Run with `HL_RPC_URL=http://localhost:3001/evm cargo test -- --ignored forwards`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires network access to an hl-node RPC"]
    async fn forwards_calls_to_an_hl_node() {
        let url = std::env::var("HL_RPC_URL")
            .unwrap_or_else(|_| "https://rpc.hyperliquid.xyz/evm".to_string());
        let forwarder = ReadPrecompileForwarder::new(&url, Handle::current(), None).unwrap();

        // Precompiles run on blocking workers, which is what the forwarder expects.
        tokio::task::spawn_blocking(move || {
            let head_hash = B256::repeat_byte(1);
            let output = forwarder.call(L1_BLOCK_NUMBER, &[], 1_000_000, 1, head_hash).unwrap();
            assert_eq!(output.bytes.len(), 32);
            assert_eq!(output.gas_used, read_precompile_gas(0, 32));

            // A gas limit one short of the cost is not enough to run the precompile.
            assert!(matches!(
                forwarder.call(L1_BLOCK_NUMBER, &[], output.gas_used - 1, 1, head_hash),
                Err(PrecompileError::OutOfGas)
            ));

            // An input the precompile rejects burns the gas limit rather than returning bytes.
            assert!(matches!(
                forwarder.call(L1_BLOCK_NUMBER, &[0xde, 0xad], 1_000_000, 1, head_hash),
                Err(PrecompileError::OutOfGas)
            ));

            // The frame that reverted in #142: a Multicall3 batch reached `bbo` on a block that
            // had recorded no call to it. Priced at 4168 gas on mainnet, which is what a direct
            // call to it costs.
            let output = forwarder.call(BBO, &BBO_INPUT, 1_000_000, 1, head_hash).unwrap();
            assert_eq!(output.bytes.len(), 64);
            assert_eq!(output.gas_used, read_precompile_gas(BBO_INPUT.len(), 64));
        })
        .await
        .unwrap();
    }
}
