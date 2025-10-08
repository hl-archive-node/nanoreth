use alloy_primitives::{BlockNumber, Bytes};
use reth_db::{table::TableInfo, tables, TableSet, TableType, TableViewer};
use std::fmt;

tables! {
    /// Read precompile calls for each block.
    table BlockReadPrecompileCalls {
        type Key = BlockNumber;
        type Value = Bytes;
    }
}
