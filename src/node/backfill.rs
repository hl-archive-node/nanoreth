//! Backfill of the block source's system tx receipts commitment
//! ([`HlExtras::system_tx_receipts_hash`]) for blocks imported before it existed, so that
//! re-executions of those blocks check their system txs too.
//!
//! Blocks are handled in chunks of [`CHUNK`] blocks:
//! - A chunk covered by the chain's embedded [`Checksums`] is backfilled locally: the system txs
//!   of its blocks are re-executed and the resulting commitments stored when their checksum
//!   matches the embedded one. Nothing is fetched.
//! - Any other chunk (above the embedded tables, with a block the block source lacks, a checksum
//!   mismatch, or in strict mode) fetches each block's commitment from the block source, checks
//!   it against a re-execution (unless disabled) and stores it when it matches.
//!
//! The node runs it in the background on start until it has covered every block imported before
//! the first run ([`SystemReceiptsBackfill::run_to_completion`]); `reth-hl
//! backfill-system-receipts` runs it offline.

use crate::{
    HlBlock, HlHeader,
    chainspec::{HlChainSpec, MAINNET_CHAIN_ID, TESTNET_CHAIN_ID},
    node::{
        HlNode,
        evm::config::HlEvmConfig,
        storage::tables,
        types::{BlockAndReceipts, HlExtras, LegacyReceipt, system_tx_receipts_hash},
    },
    pseudo_peer::{BlockSourceArgs, BlockSourceBoxed},
};
use alloy_consensus::BlockHeader;
use alloy_primitives::{B256, Bytes, Log, keccak256};
use clap::Parser;
use futures::StreamExt;
use rayon::prelude::*;
use reth_chainspec::EthChainSpec;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::common::{AccessRights, EnvironmentArgs};
use reth_db_api::transaction::{DbTx, DbTxMut};
use reth_evm::{ConfigureEvm, execute::BlockExecutor};
use reth_provider::{
    BlockHashReader, BlockNumReader, BlockReader, ChainSpecProvider, DBProvider,
    DatabaseProviderFactory, HeaderProvider, StageCheckpointReader, StageCheckpointWriter,
    StateProviderFactory, TransactionVariant, providers::BlockchainProvider,
};
use reth_revm::{State, database::StateProviderDatabase};
use reth_stages_types::{StageCheckpoint, StageId};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tracing::{error, info, warn};

/// Last block the backfill has covered.
const PROGRESS: StageId = StageId::Other("SystemReceiptsBackfill");
/// Last block imported before the first backfill run; later blocks get their commitment at
/// import.
const TARGET: StageId = StageId::Other("SystemReceiptsBackfillTarget");

/// Blocks per chunk: per database transaction, and per checksum of the embedded tables. Chunk
/// `i` is `i * CHUNK..=(i + 1) * CHUNK - 1`.
const CHUNK: u64 = 10_000;

/// Retries of a failed block fetch, with exponential backoff from 1s.
const FETCH_RETRIES: u32 = 4;

/// Default threads re-executing system txs in the node's background backfill. Re-execution
/// mostly faults in pages of the memory-mapped database, which scales poorly past a few dozen
/// threads, and the node keeps syncing meanwhile.
pub const DEFAULT_NODE_THREADS: usize = 8;

/// Providers the backfill runs on.
pub trait BackfillProvider:
    DatabaseProviderFactory<
        Provider: DBProvider
                      + BlockNumReader
                      + HeaderProvider<Header = HlHeader>
                      + BlockReader<Block = HlBlock>
                      + StageCheckpointReader,
        ProviderRW: DBProvider<Tx: DbTxMut> + BlockHashReader + StageCheckpointWriter,
    > + StateProviderFactory
    + Clone
    + Send
    + Sync
    + 'static
{
}

impl<T> BackfillProvider for T where
    T: DatabaseProviderFactory<
            Provider: DBProvider
                          + BlockNumReader
                          + HeaderProvider<Header = HlHeader>
                          + BlockReader<Block = HlBlock>
                          + StageCheckpointReader,
            ProviderRW: DBProvider<Tx: DbTxMut> + BlockHashReader + StageCheckpointWriter,
        > + StateProviderFactory
        + Clone
        + Send
        + Sync
        + 'static
{
}

/// Checksum of the system tx receipts commitments of each [`CHUNK`] of a chain, from the block
/// source's receipts: `keccak256(number_be || commitment || ...)` over the chunk's blocks with
/// system txs, in order. Chunks with a block the block source has no commitment for have none.
///
/// Encoded as the chunk size and count (u64 LE each), then per chunk a tag byte: 0 for no
/// checksum, 1 followed by the checksum, 2 for a chunk without system txs.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Checksums(Vec<Option<B256>>);

impl Checksums {
    /// The embedded table of a chain.
    fn for_chain(chain_id: u64) -> Self {
        let bytes: &[u8] = match chain_id {
            MAINNET_CHAIN_ID => include_bytes!("system_receipts_checksums/mainnet.bin"),
            TESTNET_CHAIN_ID => include_bytes!("system_receipts_checksums/testnet.bin"),
            _ => return Self::default(),
        };
        Self::decode(bytes).expect("invalid embedded system tx receipts checksums")
    }

    /// Checksum of `(number, commitment)`s.
    fn checksum(commitments: impl IntoIterator<Item = (u64, B256)>) -> B256 {
        let mut bytes = Vec::new();
        for (number, commitment) in commitments {
            bytes.extend_from_slice(&number.to_be_bytes());
            bytes.extend_from_slice(commitment.as_slice());
        }
        keccak256(bytes)
    }

    /// The checksum of `start..=end` if that is a whole chunk (block 0 aside) with one.
    fn get(&self, start: u64, end: u64) -> Option<B256> {
        let index = start / CHUNK;
        let whole = start <= (index * CHUNK).max(1) && end == index * CHUNK + CHUNK - 1;
        whole.then(|| self.0.get(index as usize).copied().flatten()).flatten()
    }

    fn encode(&self) -> Vec<u8> {
        let empty = Self::checksum([]);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&CHUNK.to_le_bytes());
        bytes.extend_from_slice(&(self.0.len() as u64).to_le_bytes());
        for checksum in &self.0 {
            match checksum {
                None => bytes.push(0),
                Some(checksum) if *checksum == empty => bytes.push(2),
                Some(checksum) => {
                    bytes.push(1);
                    bytes.extend_from_slice(checksum.as_slice());
                }
            }
        }
        bytes
    }

    fn decode(mut bytes: &[u8]) -> eyre::Result<Self> {
        fn take<'a>(bytes: &mut &'a [u8], n: usize) -> eyre::Result<&'a [u8]> {
            eyre::ensure!(bytes.len() >= n, "truncated");
            let (head, tail) = bytes.split_at(n);
            *bytes = tail;
            Ok(head)
        }
        let u64_le = |b: &[u8]| u64::from_le_bytes(b.try_into().unwrap());
        if bytes.is_empty() {
            return Ok(Self::default());
        }
        let chunk = u64_le(take(&mut bytes, 8)?);
        eyre::ensure!(chunk == CHUNK, "chunk size {chunk}, expected {CHUNK}");
        let count = u64_le(take(&mut bytes, 8)?);
        let empty = Self::checksum([]);
        let checksums = (0..count)
            .map(|_| match take(&mut bytes, 1)?[0] {
                0 => Ok(None),
                1 => Ok(Some(B256::from_slice(take(&mut bytes, 32)?))),
                2 => Ok(Some(empty)),
                tag => Err(eyre::eyre!("invalid tag {tag}")),
            })
            .collect::<eyre::Result<_>>()?;
        eyre::ensure!(bytes.is_empty(), "trailing bytes");
        Ok(Self(checksums))
    }
}

#[derive(Debug, Default)]
struct Stats {
    written: u64,
    present: u64,
    unavailable: u64,
    /// Chunks backfilled from the embedded checksums.
    local_chunks: u64,
    mismatches: Vec<u64>,
}

/// A block to backfill: number, hash and system tx count.
type Candidate = (u64, B256, usize);

/// Backfills [`HlExtras::system_tx_receipts_hash`].
pub struct SystemReceiptsBackfill<P> {
    provider: P,
    evm_config: HlEvmConfig,
    source: BlockSourceBoxed,
    checksums: Arc<Checksums>,
    pool: Arc<rayon::ThreadPool>,
    verify: bool,
}

impl<P: BackfillProvider> SystemReceiptsBackfill<P> {
    pub fn new(
        provider: P,
        chain_spec: Arc<HlChainSpec>,
        source: BlockSourceBoxed,
        threads: usize,
    ) -> eyre::Result<Self> {
        Ok(Self {
            checksums: Arc::new(Checksums::for_chain(chain_spec.chain().id())),
            provider,
            evm_config: HlEvmConfig::new(chain_spec),
            source,
            pool: Arc::new(rayon::ThreadPoolBuilder::new().num_threads(threads).build()?),
            verify: true,
        })
    }

    /// Fetches every commitment from the block source, ignoring the embedded checksums.
    pub fn strict(mut self) -> Self {
        self.checksums = Default::default();
        self
    }

    /// Stores fetched commitments without re-executing the system txs first.
    pub fn without_verification(mut self) -> Self {
        self.verify = false;
        self
    }

    /// Backfills every block imported before the first run, resuming from the stored progress.
    /// Returns immediately when that is done, or the database was empty at the first run.
    pub async fn run_to_completion(&self) -> eyre::Result<()> {
        let (progress, target) = self
            .blocking(|this| {
                let provider = this.provider.database_provider_ro()?;
                let progress = provider.get_stage_checkpoint(PROGRESS)?.map(|c| c.block_number);
                if let Some(target) = provider.get_stage_checkpoint(TARGET)? {
                    return Ok((progress.unwrap_or(0), target.block_number));
                }
                let target = provider.best_block_number()?;
                drop(provider);
                let provider = this.provider.database_provider_rw()?;
                provider.save_stage_checkpoint(TARGET, StageCheckpoint::new(target))?;
                provider.commit()?;
                Ok((progress.unwrap_or(0), target))
            })
            .await?;
        if progress >= target {
            return Ok(());
        }
        info!(from = progress + 1, to = target, "Backfilling system tx receipts commitments");
        self.run(progress + 1, target, true).await
    }

    /// Backfills `from..=to`. With `track_progress`, the progress is saved with every chunk.
    pub async fn run(&self, from: u64, to: u64, track_progress: bool) -> eyre::Result<()> {
        let mut stats = Stats::default();
        let mut start = from;
        while start <= to {
            let end = ((start / CHUNK + 1) * CHUNK - 1).min(to);
            self.run_chunk(start, end, track_progress.then_some(end), &mut stats).await?;
            info!(
                block = end,
                to,
                written = stats.written,
                already_present = stats.present,
                unavailable = stats.unavailable,
                local_chunks = stats.local_chunks,
                mismatches = stats.mismatches.len(),
                "System tx receipts backfill progress"
            );
            start = end + 1;
        }
        info!(?stats, "System tx receipts backfill finished");
        eyre::ensure!(
            stats.mismatches.is_empty(),
            "system tx receipts mismatch in {} blocks: {:?}",
            stats.mismatches.len(),
            stats.mismatches
        );
        Ok(())
    }

    async fn run_chunk(
        &self,
        start: u64,
        end: u64,
        progress: Option<u64>,
        stats: &mut Stats,
    ) -> eyre::Result<()> {
        if let Some(checksum) = self.checksums.get(start, end) {
            let local =
                self.blocking(move |this| this.backfill_locally(start, end, checksum, progress));
            match local.await? {
                Some((written, present)) => {
                    stats.written += written;
                    stats.present += present;
                    stats.local_chunks += 1;
                    return Ok(());
                }
                None => warn!(start, end, "System tx receipts checksum mismatch, fetching instead"),
            }
        }

        let (candidates, present) = self.blocking(move |this| this.candidates(start, end)).await?;
        stats.present += present;

        // Fetched one by one so that a block missing from the source (e.g. testnet gaps) is
        // skipped instead of failing the chunk.
        let fetches: Vec<_> =
            candidates.iter().map(|(number, ..)| fetch(self.source.clone(), *number)).collect();
        let blocks: Vec<_> = futures::stream::iter(fetches)
            .buffered(self.source.recommended_chunk_size() as usize)
            .collect()
            .await;

        let mut expected = Vec::with_capacity(candidates.len());
        for (candidate, block) in candidates.into_iter().zip(blocks) {
            let number = candidate.0;
            let block = match block {
                Ok(block) => block,
                Err(err) => {
                    warn!(number, %err, "Block not available from the block source");
                    stats.unavailable += 1;
                    continue;
                }
            };
            match source_commitment(&block, candidate) {
                Some(hash) => expected.push((candidate, hash)),
                None => {
                    warn!(number, "Block source has no usable system tx receipts for the block");
                    stats.unavailable += 1;
                }
            }
        }

        let (written, mismatches) =
            self.blocking(move |this| this.verify_and_write(expected, progress)).await?;
        stats.written += written;
        stats.mismatches.extend(mismatches);
        Ok(())
    }

    /// Blocks in `start..=end` with system txs, with their stored commitment if any.
    fn system_tx_blocks(
        &self,
        start: u64,
        end: u64,
    ) -> eyre::Result<Vec<(Candidate, Option<B256>)>> {
        let provider = self.provider.database_provider_ro()?;
        let mut blocks = Vec::new();
        for header in provider.sealed_headers_range(start..=end)? {
            let count = header.extras.system_tx_count as usize;
            if count == 0 {
                continue;
            }
            let number = header.number();
            let stored = read_extras(provider.tx_ref(), number)?
                .and_then(|extras| extras.system_tx_receipts_hash);
            blocks.push(((number, header.hash(), count), stored));
        }
        Ok(blocks)
    }

    /// Blocks in `start..=end` with system txs and no stored commitment, and the number of
    /// blocks that already have one.
    fn candidates(&self, start: u64, end: u64) -> eyre::Result<(Vec<Candidate>, u64)> {
        let blocks = self.system_tx_blocks(start, end)?;
        let present = blocks.iter().filter(|(_, stored)| stored.is_some()).count() as u64;
        let candidates =
            blocks.into_iter().filter(|(_, stored)| stored.is_none()).map(|(c, _)| c).collect();
        Ok((candidates, present))
    }

    /// Re-executes the blocks of the chunk `start..=end` without a stored commitment and stores
    /// their commitments if the chunk's checksum matches `checksum`. Returns the number of
    /// commitments written and already present, or `None` on a mismatch (nothing written).
    fn backfill_locally(
        &self,
        start: u64,
        end: u64,
        checksum: B256,
        progress: Option<u64>,
    ) -> eyre::Result<Option<(u64, u64)>> {
        let blocks = self.system_tx_blocks(start, end)?;
        let commitments: Vec<B256> = self.pool.install(|| {
            blocks
                .par_iter()
                .map(|&((number, _, count), stored)| match stored {
                    Some(stored) => Ok(stored),
                    None => self.execute_system_txs(number, count),
                })
                .collect::<eyre::Result<_>>()
        })?;
        let got = Checksums::checksum(
            blocks.iter().zip(&commitments).map(|(((number, ..), _), c)| (*number, *c)),
        );
        if got != checksum {
            return Ok(None);
        }
        let present = blocks.iter().filter(|(_, stored)| stored.is_some()).count() as u64;
        let missing = blocks
            .into_iter()
            .zip(commitments)
            .filter(|((_, stored), _)| stored.is_none())
            .map(|((candidate, _), commitment)| (candidate, commitment));
        Ok(Some((self.write(missing, progress)?, present)))
    }

    /// Checks each fetched commitment against a re-execution (unless disabled) and stores the
    /// matching ones. Returns the number of commitments written and the mismatching blocks.
    fn verify_and_write(
        &self,
        blocks: Vec<(Candidate, B256)>,
        progress: Option<u64>,
    ) -> eyre::Result<(u64, Vec<u64>)> {
        let executed: Vec<Option<B256>> = self.pool.install(|| {
            blocks
                .par_iter()
                .map(|&((number, _, count), _)| {
                    self.verify.then(|| self.execute_system_txs(number, count)).transpose()
                })
                .collect::<eyre::Result<_>>()
        })?;
        let mut mismatches = Vec::new();
        let matching = blocks.into_iter().zip(executed).filter_map(|((candidate, expected), got)| {
            if let Some(got) = got
                && got != expected
            {
                error!(number = candidate.0, %expected, %got, "System tx receipts mismatch");
                mismatches.push(candidate.0);
                return None;
            }
            Some((candidate, expected))
        });
        let matching: Vec<_> = matching.collect();
        Ok((self.write(matching, progress)?, mismatches))
    }

    /// Stores commitments of blocks that are still canonical, and the progress if given.
    fn write(
        &self,
        commitments: impl IntoIterator<Item = (Candidate, B256)>,
        progress: Option<u64>,
    ) -> eyre::Result<u64> {
        let provider = self.provider.database_provider_rw()?;
        let tx = provider.tx_ref();
        let mut written = 0;
        for ((number, hash, _), commitment) in commitments {
            // Skip blocks unwound or replaced since they were read.
            if provider.block_hash(number)? != Some(hash) {
                continue;
            }
            let Some(mut extras) = read_extras(tx, number)? else { continue };
            extras.system_tx_receipts_hash = Some(commitment);
            tx.put::<tables::BlockReadPrecompileCalls>(
                number,
                Bytes::from(rmp_serde::to_vec(&extras)?),
            )?;
            written += 1;
        }
        if let Some(progress) = progress {
            provider.save_stage_checkpoint(PROGRESS, StageCheckpoint::new(progress))?;
        }
        provider.commit()?;
        Ok(written)
    }

    fn execute_system_txs(&self, number: u64, system_tx_count: usize) -> eyre::Result<B256> {
        execute_system_txs(&self.provider, &self.evm_config, number, system_tx_count)
    }

    /// Runs database work off the async runtime.
    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Self) -> eyre::Result<T> + Send + 'static,
    ) -> eyre::Result<T> {
        let this = Self {
            provider: self.provider.clone(),
            evm_config: self.evm_config.clone(),
            source: self.source.clone(),
            checksums: self.checksums.clone(),
            pool: self.pool.clone(),
            verify: self.verify,
        };
        tokio::task::spawn_blocking(move || f(&this)).await?
    }
}

/// Executes the system txs of block `number` on its parent state and returns the
/// commitment to their outcomes.
fn execute_system_txs<P: BackfillProvider>(
    provider: &P,
    evm_config: &HlEvmConfig,
    number: u64,
    system_tx_count: usize,
) -> eyre::Result<B256> {
    let block = provider
        .database_provider_ro()?
        .recovered_block(number.into(), TransactionVariant::WithHash)?
        .ok_or_else(|| eyre::eyre!("block {number} not found"))?;
    let state = provider.history_by_block_number(number - 1)?;
    let mut db = State::builder()
        .with_database(StateProviderDatabase::new(state))
        .with_bundle_update()
        .build();
    let evm = evm_config.evm_with_env(&mut db, evm_config.evm_env(block.header())?);
    let ctx = evm_config.context_for_block(block.sealed_block())?;
    let mut executor = evm_config.create_executor(evm, ctx);
    executor.apply_pre_execution_changes()?;
    let mut outcomes: Vec<(bool, u64, Vec<Log>)> = Vec::with_capacity(system_tx_count);
    for tx in block.transactions_recovered().take(system_tx_count) {
        executor.execute_transaction_with_result_closure(tx, |result| {
            outcomes.push((result.is_success(), result.gas_used(), result.logs().to_vec()))
        })?;
    }
    Ok(system_tx_receipts_hash(outcomes.iter().map(|(s, g, l)| (*s, *g, l.as_slice()))))
}

/// Fetches a block from the block source, retrying transient failures (e.g. S3 throttling).
async fn fetch(source: BlockSourceBoxed, number: u64) -> eyre::Result<BlockAndReceipts> {
    let mut delay = Duration::from_secs(1);
    for _ in 0..FETCH_RETRIES {
        match source.collect_block(number).await {
            Ok(block) => return Ok(block),
            Err(err) => warn!(number, %err, ?delay, "Failed to fetch block, retrying"),
        }
        tokio::time::sleep(delay).await;
        delay *= 2;
    }
    source.collect_block(number).await
}

fn read_extras(tx: &impl DbTx, number: u64) -> eyre::Result<Option<HlExtras>> {
    Ok(tx
        .get::<tables::BlockReadPrecompileCalls>(number)?
        .map(|row| rmp_serde::from_slice::<HlExtras>(&row))
        .transpose()?)
}

/// The commitment the node would have stored when importing `block`, if the source block is the
/// candidate block.
fn source_commitment(
    block: &BlockAndReceipts,
    (number, hash, system_tx_count): Candidate,
) -> Option<B256> {
    // Same selection as the import: system txs without a receipt are not part of the block.
    let receipts: Vec<&LegacyReceipt> =
        block.system_txs.iter().filter_map(|tx| tx.receipt.as_ref()).collect();
    if block.number() != number || block.hash() != hash || receipts.len() != system_tx_count {
        return None;
    }
    LegacyReceipt::system_tx_receipts_hash(receipts)
}

/// Backfill the system tx receipts commitment of already imported blocks.
#[derive(Debug, Parser)]
pub struct BackfillSystemReceiptsCommand<C: ChainSpecParser> {
    #[command(flatten)]
    env: EnvironmentArgs<C>,

    #[command(flatten)]
    source: BlockSourceArgs,

    /// First block to backfill. Without `--from` and `--to`, finishes the node's background
    /// backfill instead.
    #[arg(long, requires = "to")]
    from: Option<u64>,

    /// Last block to backfill.
    #[arg(long, requires = "from")]
    to: Option<u64>,

    /// Fetch every commitment from the block source, also for chunks covered by the embedded
    /// checksums.
    #[arg(long)]
    strict: bool,

    /// Store fetched commitments without re-executing the system txs first. For pruned nodes,
    /// which lack the historical state.
    #[arg(long)]
    no_verify: bool,
}

impl<C: ChainSpecParser> BackfillSystemReceiptsCommand<C> {
    pub(crate) fn chain(&self) -> &Arc<C::ChainSpec> {
        &self.env.chain
    }
}

impl<C: ChainSpecParser<ChainSpec = HlChainSpec>> BackfillSystemReceiptsCommand<C> {
    pub async fn execute(self) -> eyre::Result<()> {
        let source = self
            .source
            .parse()
            .await?
            .ok_or_else(|| eyre::eyre!("a block source is required (--s3, --block-source, ...)"))?
            .create_block_source((*self.env.chain).clone())
            .await;
        let factory = self.env.init::<HlNode>(AccessRights::RW)?.provider_factory;
        info!(chain = %self.env.chain.chain(), "Backfilling system tx receipts commitments");
        let mut backfill = SystemReceiptsBackfill::new(
            BlockchainProvider::new(factory.clone())?,
            factory.chain_spec(),
            source,
            std::thread::available_parallelism()?.get(),
        )?;
        if self.strict {
            backfill = backfill.strict();
        }
        if self.no_verify {
            backfill = backfill.without_verification();
        }
        match (self.from, self.to) {
            (Some(from), Some(to)) => backfill.run(from, to, false).await,
            _ => backfill.run_to_completion().await,
        }
    }
}

/// Write the per-chunk checksums of the stored system tx receipts commitments, for embedding as
/// `src/node/system_receipts_checksums/<chain>.bin`.
///
/// Run on a node whose commitments are all from the block source (imported or backfilled in
/// strict mode). Covers the whole chunks up to `--to`.
#[derive(Debug, Parser)]
pub struct SystemReceiptsChecksumsCommand<C: ChainSpecParser> {
    #[command(flatten)]
    env: EnvironmentArgs<C>,

    /// Last block to cover, rounded down to a whole chunk. Defaults to the highest block.
    #[arg(long)]
    to: Option<u64>,

    /// Output file.
    #[arg(long, required_unless_present = "verify")]
    out: Option<PathBuf>,

    /// Instead of writing a table, check the embedded one against a re-execution of the system
    /// txs of every block it covers (as the local backfill does), ignoring stored commitments.
    #[arg(long, conflicts_with = "out")]
    verify: bool,

    /// Threads re-executing system txs with `--verify`. Defaults to the node's backfill.
    #[arg(long, default_value_t = DEFAULT_NODE_THREADS)]
    threads: usize,
}

impl<C: ChainSpecParser> SystemReceiptsChecksumsCommand<C> {
    pub(crate) fn chain(&self) -> &Arc<C::ChainSpec> {
        &self.env.chain
    }
}

impl<C: ChainSpecParser<ChainSpec = HlChainSpec>> SystemReceiptsChecksumsCommand<C> {
    pub fn execute(self) -> eyre::Result<()> {
        let factory = self.env.init::<HlNode>(AccessRights::RO)?.provider_factory;
        let provider = factory.database_provider_ro()?;
        let to = match self.to {
            Some(to) => to,
            None => provider.best_block_number()?,
        };
        let chunks = (to + 1) / CHUNK;
        if self.verify {
            return verify_embedded_checksums(factory, chunks, self.threads);
        }
        let out = self.out.expect("required without --verify");
        let checksums: Vec<Option<B256>> = (0..chunks)
            .into_par_iter()
            .map(|index| -> eyre::Result<_> {
                let provider = factory.database_provider_ro()?;
                let mut commitments = Vec::new();
                for header in provider.headers_range(index * CHUNK..=index * CHUNK + CHUNK - 1)? {
                    if header.extras.system_tx_count == 0 {
                        continue;
                    }
                    let number = header.number();
                    match read_extras(provider.tx_ref(), number)?
                        .and_then(|extras| extras.system_tx_receipts_hash)
                    {
                        Some(commitment) => commitments.push((number, commitment)),
                        None => return Ok(None),
                    }
                }
                Ok(Some(Checksums::checksum(commitments)))
            })
            .collect::<eyre::Result<_>>()?;
        let missing = checksums.iter().filter(|c| c.is_none()).count();
        let checksums = Checksums(checksums);
        std::fs::write(&out, checksums.encode())?;
        info!(chunks, missing, out = %out.display(), "Wrote system tx receipts checksums");
        Ok(())
    }
}

/// Re-executes the system txs of every block covered by the chain's embedded checksums (up to
/// chunk `chunks`) and compares each chunk's checksum.
fn verify_embedded_checksums<N>(
    factory: reth_provider::ProviderFactory<N>,
    chunks: u64,
    threads: usize,
) -> eyre::Result<()>
where
    N: reth_provider::providers::ProviderNodeTypes<
            ChainSpec = HlChainSpec,
            Primitives = crate::HlPrimitives,
        >,
    BlockchainProvider<N>: BackfillProvider,
{
    let chain_spec = factory.chain_spec();
    let table = Checksums::for_chain(chain_spec.chain().id());
    let provider = BlockchainProvider::new(factory)?;
    let evm_config = HlEvmConfig::new(chain_spec);
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build()?;
    let indices: Vec<u64> = (0..chunks.min(table.0.len() as u64))
        .filter(|&index| table.0[index as usize].is_some())
        .collect();
    let started = std::time::Instant::now();
    let (done, blocks) = (AtomicU64::new(0), AtomicU64::new(0));
    let mismatches: Vec<u64> = pool.install(|| {
        indices
            .par_iter()
            .map(|&index| -> eyre::Result<Option<u64>> {
                let (start, end) = (index * CHUNK, index * CHUNK + CHUNK - 1);
                let headers = provider.database_provider_ro()?.sealed_headers_range(start..=end)?;
                let mut commitments = Vec::new();
                for header in headers {
                    let count = header.extras.system_tx_count as usize;
                    if count > 0 {
                        let number = header.number();
                        let commitment = execute_system_txs(&provider, &evm_config, number, count)?;
                        commitments.push((number, commitment));
                    }
                }
                blocks.fetch_add(commitments.len() as u64, Ordering::Relaxed);
                let done = done.fetch_add(1, Ordering::Relaxed) + 1;
                if done % 100 == 0 {
                    info!(
                        chunks = done,
                        of = indices.len(),
                        blocks = blocks.load(Ordering::Relaxed),
                        elapsed = ?started.elapsed(),
                        "Verifying embedded system tx receipts checksums"
                    );
                }
                let matches = Some(Checksums::checksum(commitments)) == table.0[index as usize];
                if !matches {
                    error!(start, end, "Embedded system tx receipts checksum mismatch");
                }
                Ok((!matches).then_some(index))
            })
            .filter_map(Result::transpose)
            .collect::<eyre::Result<_>>()
    })?;
    info!(
        chunks = indices.len(),
        blocks = blocks.load(Ordering::Relaxed),
        mismatches = mismatches.len(),
        elapsed = ?started.elapsed(),
        "Verified embedded system tx receipts checksums"
    );
    eyre::ensure!(mismatches.is_empty(), "checksum mismatch in chunks {mismatches:?}");
    Ok(())
}

/// Starts the backfill in the background of a running node, see [`SystemReceiptsBackfill`].
pub fn spawn_on_node<P: BackfillProvider>(
    provider: P,
    chain_spec: Arc<HlChainSpec>,
    source: BlockSourceBoxed,
    strict: bool,
    threads: usize,
    executor: &reth::tasks::TaskExecutor,
) -> eyre::Result<()> {
    let mut backfill = SystemReceiptsBackfill::new(provider, chain_spec, source, threads)?;
    if strict {
        backfill = backfill.strict();
    }
    executor.spawn(Box::pin(async move {
        if let Err(err) = backfill.run_to_completion().await {
            error!(%err, "System tx receipts backfill failed");
        }
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_roundtrip() {
        let checksums = Checksums(vec![
            None,
            Some(Checksums::checksum([])),
            Some(Checksums::checksum([(5, B256::repeat_byte(1))])),
        ]);
        let encoded = checksums.encode();
        assert_eq!(encoded.len(), 16 + 1 + 1 + 33);
        assert_eq!(Checksums::decode(&encoded).unwrap(), checksums);
        assert_eq!(Checksums::decode(&[]).unwrap(), Checksums::default());
    }

    #[test]
    fn checksum_applies_to_whole_chunks_only() {
        let c = |n: u8| Some(B256::repeat_byte(n));
        let checksums = Checksums(vec![c(0), c(1), None]);
        assert_eq!(checksums.get(1, CHUNK - 1), c(0));
        assert_eq!(checksums.get(CHUNK, 2 * CHUNK - 1), c(1));
        assert_eq!(checksums.get(CHUNK + 1, 2 * CHUNK - 1), None);
        assert_eq!(checksums.get(CHUNK, 2 * CHUNK - 2), None);
        assert_eq!(checksums.get(2 * CHUNK, 3 * CHUNK - 1), None);
        assert_eq!(checksums.get(3 * CHUNK, 4 * CHUNK - 1), None);
    }

    #[test]
    fn embedded_checksums_decode() {
        Checksums::for_chain(MAINNET_CHAIN_ID);
        Checksums::for_chain(TESTNET_CHAIN_ID);
    }
}
