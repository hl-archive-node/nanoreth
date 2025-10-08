use crate::{
    chainspec::{parser::HlChainSpecParser, HlChainSpec},
    node::{consensus::HlConsensus, evm::config::HlEvmConfig, storage::tables::Tables, HlNode},
    pseudo_peer::BlockSourceArgs,
};
use clap::{Args, Parser};
use reth::{
    args::LogArgs,
    builder::{NodeBuilder, WithLaunchContext},
    cli::Commands,
    prometheus_exporter::install_prometheus_recorder,
    version::version_metadata,
    CliRunner,
};
use reth_chainspec::EthChainSpec;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::{common::EnvironmentArgs, launcher::FnLauncher};
use reth_db::{init_db, mdbx::init_db_for, DatabaseEnv};
use reth_tracing::FileWorkerGuard;
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

    /// Enable hl-node compliant mode.
    ///
    /// This option
    /// 1. filters out system transactions from block transaction list.
    /// 2. filters out logs that are not from the block's transactions.
    /// 3. filters out logs and transactions from subscription.
    #[arg(long, env = "HL_NODE_COMPLIANT")]
    pub hl_node_compliant: bool,

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
    pub command: Commands<Spec, Ext>,

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
            WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, C::ChainSpec>>,
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
            WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, C::ChainSpec>>,
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

        let components =
            |spec: Arc<C::ChainSpec>| (HlEvmConfig::new(spec.clone()), HlConsensus::new(spec));

        match self.command {
            Commands::Node(command) => runner.run_command_until_exit(|ctx| {
                command.execute(ctx, FnLauncher::new::<C, Ext>(launcher))
            }),
            Commands::Init(command) => {
                runner.run_blocking_until_ctrl_c(command.execute::<HlNode>())
            }
            Commands::InitState(command) => {
                // Need to invoke `init_db_for` to create `BlockReadPrecompileCalls` table
                Self::init_db(&command.env)?;
                runner.run_blocking_until_ctrl_c(command.execute::<HlNode>())
            }
            Commands::DumpGenesis(command) => runner.run_blocking_until_ctrl_c(command.execute()),
            Commands::Db(command) => runner.run_blocking_until_ctrl_c(command.execute::<HlNode>()),
            Commands::Stage(command) => {
                runner.run_command_until_exit(|ctx| command.execute::<HlNode, _>(ctx, components))
            }
            Commands::Config(command) => runner.run_until_ctrl_c(command.execute()),
            Commands::Recover(command) => {
                runner.run_command_until_exit(|ctx| command.execute::<HlNode>(ctx))
            }
            Commands::Prune(command) => runner.run_until_ctrl_c(command.execute::<HlNode>()),
            Commands::Import(command) => {
                runner.run_blocking_until_ctrl_c(command.execute::<HlNode, _>(components))
            }
            Commands::P2P(_command) => not_applicable!(P2P),
            Commands::ImportEra(_command) => not_applicable!(ImportEra),
            Commands::Download(_command) => not_applicable!(Download),
            Commands::ExportEra(_) => not_applicable!(ExportEra),
            Commands::ReExecute(_) => not_applicable!(ReExecute),
            #[cfg(feature = "dev")]
            Commands::TestVectors(_command) => not_applicable!(TestVectors),
        }
    }

    /// Initializes tracing with the configured options.
    ///
    /// If file logging is enabled, this function returns a guard that must be kept alive to ensure
    /// that all logs are flushed to disk.
    pub fn init_tracing(&self) -> eyre::Result<Option<FileWorkerGuard>> {
        let guard = self.logs.init_tracing()?;
        Ok(guard)
    }

    fn init_db(env: &EnvironmentArgs<C>) -> eyre::Result<()> {
        let data_dir = env.datadir.clone().resolve_datadir(env.chain.chain());
        let db_path = data_dir.db();
        init_db(db_path.clone(), env.db.database_args())?;
        init_db_for::<_, Tables>(db_path, env.db.database_args())?;
        Ok(())
    }
}
