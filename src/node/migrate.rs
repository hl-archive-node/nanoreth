use alloy_consensus::Header;
use alloy_primitives::{B256, BlockHash, Bytes, U256, b256, hex::ToHexExt};
use reth::{
    api::NodeTypesWithDBAdapter,
    args::{DatabaseArgs, DatadirArgs},
    dirs::{ChainPath, DataDirPath},
};
use reth_chainspec::EthChainSpec;
use reth_db::{
    DatabaseEnv,
    mdbx::{RO, tx::Tx},
    models::CompactU256,
    static_file::iter_static_files,
    table::Decompress,
    tables,
};
use reth_db_api::{
    cursor::{DbCursorRO, DbCursorRW},
    transaction::{DbTx, DbTxMut},
};
use reth_errors::ProviderResult;
use reth_ethereum_primitives::EthereumReceipt;
use reth::tasks::Runtime;
use reth_provider::{
    DatabaseProvider, ProviderFactory, ReceiptProvider, StaticFileProviderFactory,
    StaticFileSegment, StaticFileWriter, to_range,
    providers::{NodeTypesForProvider, RocksDBProvider, StaticFileProvider},
    static_file::SegmentRangeInclusive,
};
use std::{fs::File, io::Write, path::PathBuf, sync::Arc};
use tracing::{info, warn};

use crate::{HlHeader, HlPrimitives, chainspec::HlChainSpec};

pub(crate) trait HlNodeType:
    NodeTypesForProvider<ChainSpec = HlChainSpec, Primitives = HlPrimitives>
{
}
impl<N: NodeTypesForProvider<ChainSpec = HlChainSpec, Primitives = HlPrimitives>> HlNodeType for N {}

pub(super) struct Migrator<N: HlNodeType> {
    data_dir: ChainPath<DataDirPath>,
    provider_factory: ProviderFactory<NodeTypesWithDBAdapter<N, Arc<DatabaseEnv>>>,
}

impl<N: HlNodeType> Migrator<N> {
    const MIGRATION_PATH_SUFFIX: &'static str = "migration-tmp";

    pub fn new(
        chain_spec: HlChainSpec,
        datadir: DatadirArgs,
        database_args: DatabaseArgs,
        runtime: Runtime,
    ) -> eyre::Result<Self> {
        let data_dir = datadir.clone().resolve_datadir(chain_spec.chain());
        let provider_factory =
            Self::provider_factory(chain_spec, datadir, database_args, runtime)?;
        Ok(Self { data_dir, provider_factory })
    }

    pub fn sf_provider(&self) -> StaticFileProvider<HlPrimitives> {
        self.provider_factory.static_file_provider()
    }

    pub fn migrate_db(&self) -> eyre::Result<()> {
        let is_empty = Self::highest_block_number(&self.sf_provider()).is_none();

        if is_empty {
            return Ok(());
        }

        self.migrate_db_inner()
    }

    fn highest_block_number(sf_provider: &StaticFileProvider<HlPrimitives>) -> Option<u64> {
        sf_provider.get_highest_static_file_block(StaticFileSegment::Headers)
    }

    fn migrate_db_inner(&self) -> eyre::Result<()> {
        let migrated_mdbx = MigratorMdbx::<N>(self).migrate_mdbx()?;
        let migrated_static_files = MigrateStaticFiles::<N>(self).migrate_static_files()?;

        if migrated_mdbx || migrated_static_files {
            info!("Database migrated successfully");
        }
        Ok(())
    }

    fn conversion_tmp_dir(&self) -> PathBuf {
        self.data_dir.data_dir().join(Self::MIGRATION_PATH_SUFFIX)
    }

    fn provider_factory(
        chain_spec: HlChainSpec,
        datadir: DatadirArgs,
        database_args: DatabaseArgs,
        runtime: Runtime,
    ) -> eyre::Result<ProviderFactory<NodeTypesWithDBAdapter<N, Arc<DatabaseEnv>>>> {
        let data_dir = datadir.clone().resolve_datadir(chain_spec.chain());
        let db_env = reth_db::init_db(data_dir.db(), database_args.database_args())?;
        let static_file_provider = StaticFileProvider::read_only(data_dir.static_files())?;
        let rocksdb_provider =
            RocksDBProvider::builder(data_dir.rocksdb()).with_default_tables().build()?;
        let db = Arc::new(db_env);
        Ok(ProviderFactory::new(
            db,
            Arc::new(chain_spec),
            static_file_provider,
            rocksdb_provider,
            runtime,
        )?)
    }
}

struct MigratorMdbx<'a, N: HlNodeType>(&'a Migrator<N>);

impl<'a, N: HlNodeType> MigratorMdbx<'a, N> {
    fn migrate_mdbx(&self) -> eyre::Result<bool> {
        // if any header is in old format, we need to migrate it, so we pick the first and last one
        let db_env = self.0.provider_factory.provider()?;
        let mut cursor = db_env.tx_ref().cursor_read::<tables::Headers<Bytes>>()?;

        let migration_needed = {
            let first_is_old = match cursor.first()? {
                Some((number, header)) => using_old_header(number, &header),
                None => false,
            };
            let last_is_old = match cursor.last()? {
                Some((number, header)) => using_old_header(number, &header),
                None => false,
            };
            first_is_old || last_is_old
        };

        if !migration_needed {
            return Ok(false);
        }

        check_if_migration_enabled()?;

        self.migrate_mdbx_inner()?;
        Ok(true)
    }

    fn migrate_mdbx_inner(&self) -> eyre::Result<()> {
        // There shouldn't be many headers in mdbx, but using file for safety
        info!("Old database detected, migrating mdbx...");
        let conversion_tmp = self.0.conversion_tmp_dir();
        let tmp_path = conversion_tmp.join("headers.rmp");

        if conversion_tmp.exists() {
            std::fs::remove_dir_all(&conversion_tmp)?;
        }
        std::fs::create_dir_all(&conversion_tmp)?;

        let count = self.export_old_headers(&tmp_path)?;
        self.import_new_headers(tmp_path, count)?;
        Ok(())
    }

    fn export_old_headers(&self, tmp_path: &PathBuf) -> Result<i32, eyre::Error> {
        let db_env = self.0.provider_factory.provider()?;
        let mut cursor_read = db_env.tx_ref().cursor_read::<tables::Headers<Bytes>>()?;
        let mut tmp_writer = File::create(tmp_path)?;
        let mut count = 0;
        let old_headers = cursor_read.walk(None)?.filter_map(|row| {
            let (block_number, header) = row.ok()?;
            if !using_old_header(block_number, &header) {
                None
            } else {
                Some((block_number, Header::decompress(&header).ok()?))
            }
        });
        for (block_number, header) in old_headers {
            let receipt =
                db_env.receipts_by_block(block_number.into())?.expect("Receipt not found");
            let new_header = to_hl_header(receipt, header);
            tmp_writer.write_all(&rmp_serde::to_vec(&(block_number, new_header))?)?;
            count += 1;
        }
        Ok(count)
    }

    fn import_new_headers(&self, tmp_path: PathBuf, count: i32) -> Result<(), eyre::Error> {
        let mut tmp_reader = File::open(tmp_path)?;
        let db_env = self.0.provider_factory.provider_rw()?;
        let mut cursor_write = db_env.tx_ref().cursor_write::<tables::Headers<Bytes>>()?;
        for _ in 0..count {
            let (number, header) = rmp_serde::from_read::<_, (u64, HlHeader)>(&mut tmp_reader)?;
            cursor_write.upsert(number, &rmp_serde::to_vec(&header)?.into())?;
        }
        db_env.commit()?;
        Ok(())
    }
}

fn check_if_migration_enabled() -> Result<(), eyre::Error> {
    if std::env::var("EXPERIMENTAL_MIGRATE_DB").is_err() {
        let err_msg = concat!(
            "Detected an old database format but experimental database migration is currently disabled. ",
            "To enable migration, set EXPERIMENTAL_MIGRATE_DB=1, or alternatively, resync your node (safest option)."
        );
        warn!("{}", err_msg);
        return Err(eyre::eyre!("{}", err_msg));
    }
    Ok(())
}

struct MigrateStaticFiles<'a, N: HlNodeType>(&'a Migrator<N>);

impl<'a, N: HlNodeType> MigrateStaticFiles<'a, N> {
    fn iterate_files_for_segment(
        &self,
        block_range: SegmentRangeInclusive,
        dir: &PathBuf,
    ) -> eyre::Result<Vec<(PathBuf, String)>> {
        let prefix = StaticFileSegment::Headers.filename(&block_range);

        let entries = std::fs::read_dir(dir)?
            .map(|res| res.map(|e| e.path()))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(entries
            .into_iter()
            .filter_map(|path| {
                let file_name = path.file_name().and_then(|f| f.to_str())?;
                if file_name.starts_with(&prefix) {
                    Some((path.clone(), file_name.to_string()))
                } else {
                    None
                }
            })
            .collect())
    }

    fn create_placeholder(&self, block_range: SegmentRangeInclusive) -> eyre::Result<()> {
        // The direction is opposite here
        let src = self.0.data_dir.static_files();
        let dst = self.0.conversion_tmp_dir();

        for (src_path, file_name) in self.iterate_files_for_segment(block_range, &src)? {
            let dst_path = dst.join(file_name);
            if dst_path.exists() {
                std::fs::remove_file(&dst_path)?;
            }
            std::os::unix::fs::symlink(src_path, dst_path)?;
        }

        Ok(())
    }

    fn move_static_files_for_segment(
        &self,
        block_range: SegmentRangeInclusive,
    ) -> eyre::Result<()> {
        let src = self.0.conversion_tmp_dir();
        let dst = self.0.data_dir.static_files();

        for (src_path, file_name) in self.iterate_files_for_segment(block_range, &src)? {
            let dst_path = dst.join(file_name);
            std::fs::remove_file(&dst_path)?;
            std::fs::rename(&src_path, &dst_path)?;
        }

        // Still StaticFileProvider needs the file to exist, so we create a symlink
        self.create_placeholder(block_range)
    }

    fn migrate_static_files(&self) -> eyre::Result<bool> {
        let conversion_tmp = self.0.conversion_tmp_dir();
        let old_path = self.0.data_dir.static_files();

        if conversion_tmp.exists() {
            std::fs::remove_dir_all(&conversion_tmp)?;
        }
        std::fs::create_dir_all(&conversion_tmp)?;

        let mut all_static_files = iter_static_files(&old_path)?;
        let all_static_files =
            all_static_files.remove(StaticFileSegment::Headers).unwrap_or_default();

        let mut first = true;

        for (block_range, _tx_ranges) in all_static_files {
            let migration_needed = self.using_old_header(block_range.start())? ||
                self.using_old_header(block_range.end())?;
            if !migration_needed {
                // Create a placeholder symlink
                self.create_placeholder(block_range)?;
                continue;
            }

            if first {
                check_if_migration_enabled()?;

                info!("Old database detected, migrating static files...");
                first = false;
            }

            let sf_provider = self.0.sf_provider();
            let sf_tmp_provider = StaticFileProvider::<HlPrimitives>::read_write(&conversion_tmp)?;
            let provider = self.0.provider_factory.provider()?;
            let block_range_for_filename = sf_provider.find_fixed_range(StaticFileSegment::Headers, block_range.start());
            migrate_single_static_file(&sf_tmp_provider, &sf_provider, &provider, block_range)?;

            self.move_static_files_for_segment(block_range_for_filename)?;
        }

        Ok(!first)
    }

    fn using_old_header(&self, number: u64) -> eyre::Result<bool> {
        let sf_provider = self.0.sf_provider();
        let content = old_headers_range(&sf_provider, number..=number)?;

        let &[row] = &content.as_slice() else {
            warn!("No header found for block {}", number);
            return Ok(false);
        };

        Ok(using_old_header(number, &row[0]))
    }
}

// Problem is that decompress just panics when the header is not valid
// So we need heuristics...
fn is_old_header(header: &[u8]) -> bool {
    const SHA3_UNCLE_OFFSET: usize = 0x24;
    const SHA3_UNCLE_HASH: B256 =
        b256!("1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347");
    const GENESIS_PREFIX: [u8; 4] = [0x01, 0x20, 0x00, 0xf8];
    let Some(sha3_uncle_hash) = header.get(SHA3_UNCLE_OFFSET..SHA3_UNCLE_OFFSET + 32) else {
        return false;
    };
    if sha3_uncle_hash == SHA3_UNCLE_HASH {
        return true;
    }

    // genesis block might be different
    if header.starts_with(&GENESIS_PREFIX) {
        return true;
    }

    false
}

fn is_new_header(header: &[u8]) -> bool {
    rmp_serde::from_slice::<HlHeader>(header).is_ok()
}

fn migrate_single_static_file<N: HlNodeType>(
    sf_out: &StaticFileProvider<HlPrimitives>,
    sf_in: &StaticFileProvider<HlPrimitives>,
    provider: &DatabaseProvider<Tx<RO>, NodeTypesWithDBAdapter<N, Arc<DatabaseEnv>>>,
    block_range: SegmentRangeInclusive,
) -> Result<(), eyre::Error> {
    info!("Migrating block range {}...", block_range);

    // block_ranges into chunks of 50000 blocks
    const CHUNK_SIZE: u64 = 50000;
    for chunk in (block_range.start()..=block_range.end()).step_by(CHUNK_SIZE as usize) {
        let end = std::cmp::min(chunk + CHUNK_SIZE - 1, block_range.end());
        let block_range = chunk..=end;
        let headers = old_headers_range(sf_in, block_range.clone())?;
        let receipts = provider.receipts_by_block_range(block_range.clone())?;
        assert_eq!(headers.len(), receipts.len());
        let mut writer = sf_out.get_writer(*block_range.start(), StaticFileSegment::Headers)?;
        let new_headers = std::iter::zip(headers, receipts)
            .map(|(header, receipts)| {
                let eth_header = Header::decompress(&header[0]).unwrap();
                let hl_header = to_hl_header(receipts, eth_header);

                let difficulty: U256 = CompactU256::decompress(&header[1]).unwrap().into();
                let hash = BlockHash::decompress(&header[2]).unwrap();
                (hl_header, difficulty, hash)
            })
            .collect::<Vec<_>>();
        for header in new_headers {
            writer.append_header(&header.0, &header.2)?;
        }
        writer.commit().unwrap();
        info!("Migrated block range {:?}...", block_range);
    }
    Ok(())
}

fn to_hl_header(receipts: Vec<EthereumReceipt>, eth_header: Header) -> HlHeader {
    let system_tx_count = receipts.iter().filter(|r| r.cumulative_gas_used == 0).count();
    HlHeader::from_ethereum_header(eth_header, &receipts, system_tx_count as u64)
}

fn old_headers_range(
    provider: &StaticFileProvider<HlPrimitives>,
    block_range: impl std::ops::RangeBounds<u64>,
) -> ProviderResult<Vec<Vec<Vec<u8>>>> {
    Ok(provider
        .fetch_range_with_predicate(
            StaticFileSegment::Headers,
            to_range(block_range),
            |cursor, number| {
                cursor.get(number.into(), 0b111).map(|rows| {
                    rows.map(|columns| columns.into_iter().map(|column| column.to_vec()).collect())
                })
            },
            |_| true,
        )?
        .into_iter()
        .collect())
}


fn using_old_header(number: u64, header: &[u8]) -> bool {
    let deserialized_old = is_old_header(header);
    let deserialized_new = is_new_header(header);

    assert!(
        deserialized_old ^ deserialized_new,
        "Header is not valid: {} {}\ndeserialized_old: {}\ndeserialized_new: {}",
        number,
        header.encode_hex(),
        deserialized_old,
        deserialized_new
    );
    deserialized_old && !deserialized_new
}
