use crate::node::{
    spot_meta::{SpotId, erc20_contract_to_spot_token},
    storage::tables::{self, SPOT_METADATA_KEY},
    types::reth_compat,
};
use alloy_primitives::Address;
use reth_db::{DatabaseEnv, cursor::DbCursorRO};
use reth_db_api::{
    Database,
    transaction::{DbTx, DbTxMut},
};
use std::{collections::BTreeMap, sync::Arc};
use tracing::info;

/// Load spot metadata from database and initialize cache
pub fn load_spot_metadata_cache(db: &Arc<DatabaseEnv>, chain_id: u64) {
    // Try to read from database
    let data = match db.view(|tx| -> Result<Option<Vec<u8>>, reth_db::DatabaseError> {
        let mut cursor = tx.cursor_read::<tables::SpotMetadata>()?;
        Ok(cursor.seek_exact(SPOT_METADATA_KEY)?.map(|(_, data)| data.to_vec()))
    }) {
        Ok(Ok(data)) => data,
        Ok(Err(e)) => {
            info!(
                "Failed to read spot metadata from database: {}. Will fetch on-demand from API.",
                e
            );
            return;
        }
        Err(e) => {
            info!(
                "Database view error while loading spot metadata: {}. Will fetch on-demand from API.",
                e
            );
            return;
        }
    };

    // Check if data exists
    let Some(data) = data else {
        info!(
            "No spot metadata found in database for chain {}. Run 'init-state' to populate, or it will be fetched on-demand from API.",
            chain_id
        );
        return;
    };

    // Deserialize metadata
    let serializable_map = match rmp_serde::from_slice::<BTreeMap<Address, u64>>(&data) {
        Ok(map) => map,
        Err(e) => {
            info!("Failed to deserialize spot metadata: {}. Will fetch on-demand from API.", e);
            return;
        }
    };

    // Convert and initialize cache
    let metadata: BTreeMap<Address, SpotId> =
        serializable_map.into_iter().map(|(addr, index)| (addr, SpotId { index })).collect();

    info!("Loaded spot metadata from database ({} entries)", metadata.len());
    reth_compat::initialize_spot_metadata_cache(metadata);
}

/// Initialize spot metadata in database from API
pub fn init_spot_metadata(
    db_path: impl AsRef<std::path::Path>,
    db_args: reth_db::mdbx::DatabaseArguments,
    chain_id: u64,
) -> eyre::Result<()> {
    info!("Initializing spot metadata for chain {}", chain_id);

    let db = Arc::new(reth_db::open_db(db_path.as_ref(), db_args)?);

    // Check if spot metadata already exists
    let exists = db.view(|tx| -> Result<bool, reth_db::DatabaseError> {
        let mut cursor = tx.cursor_read::<tables::SpotMetadata>()?;
        Ok(cursor.seek_exact(SPOT_METADATA_KEY)?.is_some())
    })??;

    if exists {
        info!("Spot metadata already exists in database");
        return Ok(());
    }

    // Fetch from API
    let metadata = match erc20_contract_to_spot_token(chain_id) {
        Ok(m) => m,
        Err(e) => {
            info!("Failed to fetch spot metadata from API: {}. Will be fetched on-demand.", e);
            return Ok(());
        }
    };

    // Store to database
    reth_compat::store_spot_metadata(&db, &metadata)?;

    info!("Successfully fetched and stored spot metadata for chain {}", chain_id);
    Ok(())
}

/// Clear the spot metadata table from the database.
///
/// Debug utility: removes all stored spot metadata so it will be re-fetched from
/// the API on the next run (e.g. after a bad fetch or to pick up overrides).
pub fn clear_spot_metadata(
    db_path: impl AsRef<std::path::Path>,
    db_args: reth_db::mdbx::DatabaseArguments,
) -> eyre::Result<()> {
    let db = Arc::new(reth_db::open_db(db_path.as_ref(), db_args)?);

    db.update(|tx| -> Result<(), reth_db::DatabaseError> {
        tx.clear::<tables::SpotMetadata>()?;
        Ok(())
    })??;

    info!("Cleared spot metadata table");
    Ok(())
}
