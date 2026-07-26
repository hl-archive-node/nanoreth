//! revmc JIT backend for HL execution.
//!
//! Compilation is a process-wide resource: the backend owns worker threads and a cache of compiled
//! code keyed by bytecode hash, and is shared by every EVM the node builds. It is installed once
//! during startup and read from there, which keeps `HlEvmFactory` constructible from the many
//! places that build one with no configuration to hand.
//!
//! When the `jit` feature is off, or the backend was never installed, or the node was started with
//! `--hl.jit false`, [`backend`] hands back a disabled backend and execution runs on the
//! interpreter exactly as before.

#[cfg(feature = "jit")]
use std::sync::OnceLock;

#[cfg(feature = "jit")]
pub use reth_evm_ethereum::factory::JitBackend;

/// Mainnet blocks below this height execute `BLOCKHASH` through a patched instruction handler
/// (see [`crate::evm::api::patch`]).
///
/// JIT-compiled code runs from revmc's own translation of the bytecode and never consults the
/// interpreter's instruction table, so the patch would silently not apply. Those blocks therefore
/// always execute on the interpreter.
pub const NON_PLACEHOLDER_BLOCK_HASH_HEIGHT: u64 = 243_538;

#[cfg(feature = "jit")]
static BACKEND: OnceLock<JitBackend> = OnceLock::new();

/// Installs the process-wide JIT backend. Later calls are ignored.
#[cfg(feature = "jit")]
pub fn install(backend: JitBackend) {
    if BACKEND.set(backend).is_err() {
        tracing::debug!(target: "reth::cli", "JIT backend already installed");
    }
}

/// Returns the installed backend, or a disabled one when JIT is unavailable or switched off.
#[cfg(feature = "jit")]
pub fn backend() -> JitBackend {
    BACKEND.get().cloned().unwrap_or_else(JitBackend::disabled)
}

/// Returns the backend to use for a block, disabling JIT where HL patches the instruction table.
#[cfg(feature = "jit")]
pub fn backend_for_block(chain_id: u64, block_number: u64) -> JitBackend {
    if chain_id == crate::chainspec::MAINNET_CHAIN_ID
        && block_number < NON_PLACEHOLDER_BLOCK_HASH_HEIGHT
    {
        return JitBackend::disabled();
    }
    backend()
}

/// Builds and installs the backend from CLI settings.
///
/// Returns `Ok(false)` when JIT stays off, either because it was disabled or because the binary
/// was built without the feature. Compilation runs on worker threads, so a failure to start the
/// backend is reported rather than silently degrading to the interpreter.
#[cfg(feature = "jit")]
pub fn start(
    enabled: bool,
    hot_threshold: Option<usize>,
    worker_count: Option<usize>,
) -> eyre::Result<bool> {
    use reth_evm_ethereum::factory::{RuntimeConfig, RuntimeTuning};

    if !enabled {
        install(JitBackend::disabled());
        return Ok(false);
    }

    let defaults = RuntimeTuning::default();
    let tuning = RuntimeTuning {
        jit_hot_threshold: hot_threshold.unwrap_or(defaults.jit_hot_threshold),
        jit_worker_count: worker_count.unwrap_or(defaults.jit_worker_count),
        ..defaults
    };
    let config = RuntimeConfig { enabled: true, tuning, ..RuntimeConfig::default() };

    let backend = JitBackend::new(config)?;
    tracing::warn!(
        target: "reth::cli",
        hot_threshold = tuning.jit_hot_threshold,
        workers = tuning.jit_worker_count,
        "Started experimental revmc JIT backend; this may cause instability",
    );
    install(backend);
    Ok(true)
}

#[cfg(not(feature = "jit"))]
pub fn start(
    enabled: bool,
    _hot_threshold: Option<usize>,
    _worker_count: Option<usize>,
) -> eyre::Result<bool> {
    if enabled {
        tracing::warn!(
            target: "reth::cli",
            "--hl.jit was requested but this binary was built without the `jit` feature",
        );
    }
    Ok(false)
}
