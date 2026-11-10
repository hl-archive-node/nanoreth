use revm::primitives::hardfork::SpecId;

#[repr(u8)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum HlSpecId {
    /// Placeholder for evm cancun fork
    #[default]
    V1,
    /// MRB precompile accounting and Osaka ModExp.
    V2,
}

impl HlSpecId {
    pub const fn into_eth_spec(self) -> SpecId {
        match self {
            Self::V1 | Self::V2 => SpecId::CANCUN,
        }
    }
}

impl From<HlSpecId> for SpecId {
    /// Converts the [`HlSpecId`] into a [`SpecId`].
    fn from(spec: HlSpecId) -> Self {
        spec.into_eth_spec()
    }
}
