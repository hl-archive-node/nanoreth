use crate::node::spot_meta::SpotId;
use alloy_primitives::{Address, address};
use std::collections::BTreeMap;

/// Testnet-specific fix for #67
pub(super) fn patch_testnet_spot_meta(map: &mut BTreeMap<Address, SpotId>) {
    map.insert(address!("0xd9cbec81df392a88aeff575e962d149d57f4d6bc"), SpotId { index: 0 });
    map.insert(address!("0x2b3370ee501b4a559b57d449569354196457d8ab"), SpotId { index: 0 });
}
