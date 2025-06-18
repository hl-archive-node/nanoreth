use std::{collections::BTreeMap, sync::Arc};

use alloy_primitives::{Address, Bytes};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
pub struct ReadPrecompileInput {
    pub input: Bytes,
    pub gas_limit: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReadPrecompileResult {
    Ok { gas_used: u64, bytes: Bytes },
    OutOfGas,
    Error,
    UnexpectedError,
}

pub type PrecompilesCache =
    Arc<Mutex<BTreeMap<u64, Vec<(Address, Vec<(ReadPrecompileInput, ReadPrecompileResult)>)>>>>;
