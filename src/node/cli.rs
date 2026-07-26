use crate::{
    chainspec::{HlChainSpec, parser::HlChainSpecParser},
    node::{
        HlNode, consensus::HlConsensus, evm::config::HlEvmConfig, migrate::Migrator,
        spot_meta::init as spot_meta_init, storage::tables::Tables,
    },
    pseudo_peer::BlockSourceArgs,
};
use clap::{Args, Parser, Subcommand};
use reth::{
    CliRunner,
    args::{DatabaseArgs, DatadirArgs, LogArgs},
    builder::{NodeBuilder, WithLaunchContext},
    cli::Commands,
    prometheus_exporter::install_prometheus_recorder,
    version::version_metadata,
};
use reth_chainspec::EthChainSpec;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::{common::EnvironmentArgs, launcher::FnLauncher};
use reth_db::{DatabaseEnv, init_db, mdbx::init_db_for};
use reth_tracing::TracingGuards;
use std::{
    fmt::{self},
    sync::Arc,
};
use tracing::info;

macro_rules! not_applicable {
    ($command:ident) => {
        todo!("{} is not applicable for HL", stringify!($command))
    };
}

#[derive(Debug, Clone, Args)]
#[non_exhaustive]
pub struct HlNodeArgs {
    #[command(flatten)]
    pub block_source_args: BlockSourceArgs,

    /// Debug cutoff height.
    ///
    /// This option is used to cut off the block import at a specific height.
    #[arg(long, env = "DEBUG_CUTOFF_HEIGHT")]
    pub debug_cutoff_height: Option<u64>,

    /// Upstream RPC URL to forward incoming transactions.
    ///
    /// Default to Hyperliquid's RPC URL when not provided (https://rpc.hyperliquid.xyz/evm).
    #[arg(long, env = "UPSTREAM_RPC_URL")]
    pub upstream_rpc_url: Option<String>,

    /// Enable JIT compilation of hot EVM bytecode (revmc).
    ///
    /// Enabled by default. Pass `--hl.jit false` to execute everything on the interpreter.
    /// Has no effect unless the binary was built with the `jit` feature.
    ///
    /// Mainnet blocks below the `BLOCKHASH` patch height always run on the interpreter, since
    /// JIT-compiled code does not consult the patched instruction table.
    #[arg(
        id = "hl.jit",
        long = "hl.jit",
        env = "HL_JIT",
        default_value_t = true,
        action = clap::ArgAction::Set
    )]
    pub jit: bool,

    /// Number of observed executions before a contract is promoted to JIT compilation.
    #[arg(long = "hl.jit.hot-threshold", env = "HL_JIT_HOT_THRESHOLD")]
    pub jit_hot_threshold: Option<usize>,

    /// Number of JIT compilation worker threads.
    #[arg(long = "hl.jit.worker-count", env = "HL_JIT_WORKER_COUNT")]
    pub jit_worker_count: Option<usize>,

    /// Enable hl-node compliant mode.
    ///
    /// This option
    /// 1. filters out system transactions from block transaction list.
    /// 2. filters out logs that are not from the block's transactions.
    /// 3. filters out logs and transactions from subscription.
    #[arg(long, env = "HL_NODE_COMPLIANT")]
    pub hl_node_compliant: bool,

    /// Enable per-request HL compliance mode via `?hl=true` or `?hl=false` query parameter.
    ///
    /// When enabled, the HTTP RPC server is restarted with a middleware layer that reads
    /// the `hl` query parameter from each request. The `--hl-node-compliant` flag controls
    /// the default behavior when no query parameter is provided (defaults to no filtering).
    #[arg(long, env = "HL_NODE_COMPLIANT_MULTIPLEXED")]
    pub hl_node_compliant_multiplexed: bool,

    /// Forward eth_call and eth_estimateGas to the upstream RPC.
    ///
    /// This is useful when read precompile is needed for gas estimation.
    #[arg(long, env = "FORWARD_CALL")]
    pub forward_call: bool,

    /// Experimental: enables the eth_getProof RPC method.
    ///
    /// Note: Due to the state root difference, trie updates* may not function correctly in all
    /// scenarios. For example, incremental root updates are not possible, which can cause
    /// eth_getProof to malfunction in some cases.
    ///
    /// This limitation does not impact normal node functionality, except for state root (which is
    /// unused) and eth_getProof. The archival state is maintained by block order, not by trie
    /// updates. As a precaution, nanoreth disables eth_getProof by default to prevent
    /// potential issues.
    ///
    /// Use --experimental-eth-get-proof to forcibly enable eth_getProof, assuming trie updates are
    /// working as intended. Enabling this by default will be tracked in #15.
    ///
    /// * Refers to the Merkle trie used for eth_getProof and state root, not actual state values.
    #[arg(long, env = "EXPERIMENTAL_ETH_GET_PROOF")]
    pub experimental_eth_get_proof: bool,

    /// Allow network configuration overrides from CLI.
    ///
    /// When enabled, network settings (discovery_addr, listener_addr, dns_discovery, nat)
    /// will be taken from CLI arguments instead of being hardcoded to localhost-only defaults.
    #[arg(long, env = "ALLOW_NETWORK_OVERRIDES")]
    pub allow_network_overrides: bool,

    /// Enable the sync server RPC endpoints (hl_syncGetBlock, hl_syncLatestBlockNumber).
    ///
    /// When enabled, this node can serve blocks to other nanoreth nodes
    /// that use --block-source=rpc://... to sync from this node.
    #[arg(long, env = "ENABLE_SYNC_SERVER")]
    pub enable_sync_server: bool,

    /// Custom API URL used when fetching spot metadata.
    ///
    /// Defaults to Hyperliquid's mainnet/testnet info endpoint based on the chain id.
    /// Useful for pointing at a proxy or a private archive of the spot-meta response.
    #[arg(long = "spot-meta.url", env = "SPOT_META_URL")]
    pub spot_meta_url: Option<String>,

    /// Manual spot-metadata overrides as `address:index` pairs.
    ///
    /// Applied on top of (and winning over) the fetched metadata and built-in patches.
    /// May be repeated or comma-separated, e.g.
    /// `--spot-meta.override 0xabc...:0 --spot-meta.override 0xdef...:3`.
    #[arg(long = "spot-meta.override", env = "SPOT_META_OVERRIDES", value_delimiter = ',')]
    pub spot_meta_overrides: Vec<String>,
}

/// reth_hl cli commands: the built-in reth subcommands plus reth_hl-specific extras.
#[derive(Debug, Subcommand)]
pub enum HlCommands<
    Spec: ChainSpecParser = HlChainSpecParser,
    Ext: clap::Args + fmt::Debug = HlNodeArgs,
> {
    /// Built-in reth subcommands (node, init, init-state, db, ...).
    #[command(flatten)]
    Reth(Commands<Spec, Ext>),

    /// Clear the spot metadata table from the database (debug utility).
    ///
    /// Removes all stored spot metadata so it will be re-fetched from the API on
    /// the next run. Useful after a bad fetch or to re-apply `--spot-meta.*` overrides.
    #[command(name = "clear-spot-meta")]
    ClearSpotMeta(ClearSpotMetaCommand<Spec>),
}

impl<C: ChainSpecParser, Ext: clap::Args + fmt::Debug> HlCommands<C, Ext> {
    /// Returns the underlying chain being used for commands, if any.
    pub fn chain_spec(&self) -> Option<&Arc<C::ChainSpec>> {
        match self {
            Self::Reth(command) => command.chain_spec(),
            Self::ClearSpotMeta(command) => Some(&command.env.chain),
        }
    }
}

/// Clear the spot metadata table from the database.
#[derive(Debug, Parser)]
pub struct ClearSpotMetaCommand<C: ChainSpecParser> {
    #[command(flatten)]
    env: EnvironmentArgs<C>,
}

impl<C: ChainSpecParser<ChainSpec = HlChainSpec>> ClearSpotMetaCommand<C> {
    fn execute(self) -> eyre::Result<()> {
        let data_dir = self.env.datadir.clone().resolve_datadir(self.env.chain.chain());
        let db_path = data_dir.db();
        spot_meta_init::clear_spot_metadata(db_path, self.env.db.database_args())
    }
}

/// The main reth_hl cli interface.
///
/// This is the entrypoint to the executable.
#[derive(Debug, Parser)]
#[command(author, version =version_metadata().short_version.as_ref(), long_version = version_metadata().long_version.as_ref(), about = "Reth", long_about = None)]
pub struct Cli<Spec: ChainSpecParser = HlChainSpecParser, Ext: clap::Args + fmt::Debug = HlNodeArgs>
{
    /// The command to run
    #[command(subcommand)]
    pub command: HlCommands<Spec, Ext>,

    #[command(flatten)]
    logs: LogArgs,
}

impl<C, Ext> Cli<C, Ext>
where
    C: ChainSpecParser<ChainSpec = HlChainSpec>,
    Ext: clap::Args + fmt::Debug,
{
    /// Execute the configured cli command.
    ///
    /// This accepts a closure that is used to launch the node via the
    /// [`NodeCommand`](reth_cli_commands::node::NodeCommand).
    pub fn run(
        self,
        launcher: impl AsyncFnOnce(
            WithLaunchContext<NodeBuilder<DatabaseEnv, C::ChainSpec>>,
            Ext,
        ) -> eyre::Result<()>,
    ) -> eyre::Result<()> {
        self.with_runner(CliRunner::try_default_runtime()?, launcher)
    }

    /// Execute the configured cli command with the provided [`CliRunner`].
    pub fn with_runner(
        mut self,
        runner: CliRunner,
        launcher: impl AsyncFnOnce(
            WithLaunchContext<NodeBuilder<DatabaseEnv, C::ChainSpec>>,
            Ext,
        ) -> eyre::Result<()>,
    ) -> eyre::Result<()> {
        // Add network name if available to the logs dir
        if let Some(chain_spec) = self.command.chain_spec() {
            self.logs.log_file_directory =
                self.logs.log_file_directory.join(chain_spec.chain().to_string());
        }

        let _guard = self.init_tracing()?;
        info!(target: "reth::cli", "Initialized tracing, debug log directory: {}", self.logs.log_file_directory);

        // Install the prometheus recorder to be sure to record all metrics
        let _ = install_prometheus_recorder();

        let components = |spec: Arc<C::ChainSpec>| {
            (HlEvmConfig::new(spec.clone()), Arc::new(HlConsensus::new(spec)))
        };

        let rt = runner.runtime();

        // Handle reth_hl-specific commands; otherwise fall through to the built-in
        // reth subcommands below.
        let command = match self.command {
            HlCommands::Reth(command) => command,
            HlCommands::ClearSpotMeta(command) => return command.execute(),
        };

        match command {
            Commands::Node(command) => runner.run_command_until_exit(|ctx| {
                // NOTE: This is for one time migration around Oct 10 upgrade:
                // It's not necessary anymore, an environment variable gate is added here.
                if std::env::var("CHECK_DB_MIGRATION").is_ok() {
                    Self::migrate_db(&command.chain, &command.datadir, &command.db, rt.clone())
                        .expect("Failed to migrate database");
                }
                command.execute(ctx, FnLauncher::new::<C, Ext>(launcher))
            }),
            Commands::Init(command) => {
                runner.run_blocking_until_ctrl_c(command.execute::<HlNode>(rt))
            }
            Commands::InitState(command) => {
                // Validate file paths early with clear error messages.
                // On Linux, File::open() succeeds on directories, then read_to_end()
                // fails with a cryptic "Is a directory" error.
                if command.without_evm
                    && let Some(ref path) = command.header
                {
                    if path.is_dir() {
                        return Err(eyre::eyre!(
                            "--header path '{}' is a directory, not a file. \
                             If using Docker, ensure the source file exists on the host \
                             (missing source files cause Docker to create directories \
                             at the mount point).",
                            path.display()
                        ));
                    }
                    if !path.exists() {
                        return Err(eyre::eyre!(
                            "--header path '{}' does not exist",
                            path.display()
                        ));
                    }
                }
                if command.state.is_dir() {
                    return Err(eyre::eyre!(
                        "State dump path '{}' is a directory, not a file. \
                         If using Docker, ensure the source file exists on the host \
                         (missing source files cause Docker to create directories \
                         at the mount point).",
                        command.state.display()
                    ));
                }
                if !command.state.exists() {
                    return Err(eyre::eyre!(
                        "State dump path '{}' does not exist",
                        command.state.display()
                    ));
                }
                // Need to invoke `init_db_for` to create `BlockReadPrecompileCalls` table
                Self::init_db(&command.env)?;
                runner.run_blocking_until_ctrl_c(command.execute::<HlNode>(rt))
            }
            Commands::DumpGenesis(command) => runner.run_blocking_until_ctrl_c(command.execute()),
            Commands::Db(command) => {
                runner.run_blocking_command_until_exit(|ctx| command.execute::<HlNode>(ctx))
            }
            Commands::Stage(command) => {
                runner.run_command_until_exit(|ctx| command.execute::<HlNode, _>(ctx, components))
            }
            Commands::Config(command) => runner.run_until_ctrl_c(command.execute()),
            Commands::Prune(command) => {
                runner.run_command_until_exit(|ctx| command.execute::<HlNode>(ctx))
            }
            Commands::Import(command) => {
                runner.run_blocking_until_ctrl_c(command.execute::<HlNode, _>(components, rt))
            }
            Commands::P2P(_command) => not_applicable!(P2P),
            Commands::ImportEra(_command) => not_applicable!(ImportEra),
            Commands::Download(_command) => not_applicable!(Download),
            Commands::ExportEra(_) => not_applicable!(ExportEra),
            Commands::ReExecute(_) => not_applicable!(ReExecute),
            Commands::SnapshotManifest(_) => not_applicable!(SnapshotManifest),
            #[cfg(feature = "dev")]
            Commands::TestVectors(_command) => not_applicable!(TestVectors),
        }
    }

    /// Initializes tracing with the configured options.
    ///
    /// If file logging is enabled, this function returns a guard that must be kept alive to ensure
    /// that all logs are flushed to disk.
    pub fn init_tracing(&self) -> eyre::Result<TracingGuards> {
        let guard = self.logs.init_tracing()?;
        Ok(guard)
    }

    fn init_db(env: &EnvironmentArgs<C>) -> eyre::Result<()> {
        let data_dir = env.datadir.clone().resolve_datadir(env.chain.chain());
        let db_path = data_dir.db();
        init_db(db_path.clone(), env.db.database_args())?;
        init_db_for::<_, Tables>(db_path.clone(), env.db.database_args())?;

        // Initialize spot metadata in database
        let chain_id = env.chain.chain().id();
        spot_meta_init::init_spot_metadata(db_path, env.db.database_args(), chain_id)?;

        Ok(())
    }

    fn migrate_db(
        chain: &HlChainSpec,
        datadir: &DatadirArgs,
        db: &DatabaseArgs,
        runtime: reth::tasks::Runtime,
    ) -> eyre::Result<()> {
        Migrator::<HlNode>::new(chain.clone(), datadir.clone(), *db, runtime)?.migrate_db()?;
        Ok(())
    }
}
