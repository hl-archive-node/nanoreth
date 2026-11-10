use super::{HlEvm, mrb};
use crate::evm::{
    api::{
        builder::HlBuilder,
        ctx::{DefaultHl, HlContext},
    },
    spec::HlSpecId,
    transaction::HlTxEnv,
};
use alloy_primitives::Address;
use reth_evm::{
    Database, EvmEnv, EvmFactory,
    precompiles::{DynPrecompile, Precompile, PrecompileInput, PrecompilesMap},
};
use reth_revm::Context;
use revm::{
    Inspector,
    context::{
        TxEnv,
        result::{EVMError, HaltReason},
    },
    inspector::NoOpInspector,
    precompile::{PrecompileError, PrecompileId, PrecompileResult, PrecompileSpecId, Precompiles},
};

/// Factory producing [`HlEvm`].
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct HlEvmFactory;

impl EvmFactory for HlEvmFactory {
    type Evm<DB: Database, I: Inspector<HlContext<DB>>> = HlEvm<DB, I, Self::Precompiles>;
    type Context<DB: Database> = HlContext<DB>;
    type Tx = HlTxEnv<TxEnv>;
    type Error<DBError: core::error::Error + Send + Sync + 'static> = EVMError<DBError>;
    type HaltReason = HaltReason;
    type Spec = HlSpecId;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<HlSpecId>,
    ) -> Self::Evm<DB, NoOpInspector> {
        let spec_id = *input.spec_id();
        HlEvm {
            inner: Context::hl()
                .with_block(input.block_env)
                .with_cfg(input.cfg_env)
                .with_db(db)
                .build_hl_with_inspector(NoOpInspector {})
                .with_precompiles(hl_precompiles(spec_id)),
            inspect: false,
        }
    }

    fn create_evm_with_inspector<
        DB: Database<Error: Send + Sync + 'static>,
        I: Inspector<Self::Context<DB>>,
    >(
        &self,
        db: DB,
        input: EvmEnv<HlSpecId>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let spec_id = *input.spec_id();
        HlEvm {
            inner: Context::hl()
                .with_block(input.block_env)
                .with_cfg(input.cfg_env)
                .with_db(db)
                .build_hl_with_inspector(inspector)
                .with_precompiles(hl_precompiles(spec_id)),
            inspect: true,
        }
    }
}

fn hl_precompiles(spec_id: HlSpecId) -> PrecompilesMap {
    let spec = PrecompileSpecId::from_spec_id(spec_id.into());
    let builtin = Precompiles::new(spec);
    let mut precompiles = PrecompilesMap::from_static(builtin);
    if spec_id == HlSpecId::V2 {
        apply_mrb_precompiles(&mut precompiles, builtin);
    }
    precompiles
}

fn apply_mrb_precompiles(precompiles: &mut PrecompilesMap, builtin: &Precompiles) {
    let osaka = &revm::precompile::modexp::OSAKA;
    let id = osaka.id().clone();
    let implementation = *osaka.precompile();
    precompiles
        .apply_precompile(osaka.address(), |_| Some(DynPrecompile::from((id, implementation))));

    for address in mrb::affected_precompile_addresses() {
        // Going dynamic renames every precompile to `Custom("closure")`; restore the builtin id.
        let id = builtin.get(&address).map(|precompile| precompile.id().clone());
        wrap_mrb_precompile(precompiles, address, id);
    }
}

fn wrap_mrb_precompile(
    precompiles: &mut PrecompilesMap,
    address: Address,
    id: Option<PrecompileId>,
) {
    let multiplier = mrb::precompile_multiplier(address);
    precompiles.apply_precompile(&address, |precompile| {
        precompile.map(|precompile| mrb_precompile(precompile, id, multiplier))
    });
}

fn mrb_precompile(
    precompile: DynPrecompile,
    id: Option<PrecompileId>,
    multiplier: u64,
) -> DynPrecompile {
    let id = id.unwrap_or_else(|| precompile.precompile_id().clone());
    DynPrecompile::from((id, move |input: PrecompileInput<'_>| {
        run_mrb_precompile(&precompile, input, multiplier)
    }))
}

/// Runs `precompile` with `multiplier` times its gas price.
///
/// Dividing the gas limit before the call and multiplying the result after it fails exactly when
/// `multiplier * cost > gas_limit`, since precompile costs are integers.
fn run_mrb_precompile(
    precompile: &DynPrecompile,
    mut input: PrecompileInput<'_>,
    multiplier: u64,
) -> PrecompileResult {
    input.gas /= multiplier;
    let mut output = precompile.call(input)?;
    output.gas_used = output.gas_used.checked_mul(multiplier).ok_or(PrecompileError::OutOfGas)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_evm::{EvmInternals, eth::EthEvmContext};
    use alloy_primitives::{U256, hex};
    use revm::{
        database::EmptyDB,
        precompile::{bn254, modexp, secp256k1},
    };

    fn call(spec_id: HlSpecId, address: Address, data: &[u8], gas: u64) -> PrecompileResult {
        let precompiles = hl_precompiles(spec_id);
        let precompile = precompiles.get(&address).unwrap();
        let mut ctx = EthEvmContext::new(EmptyDB::default(), Default::default());
        precompile.call(PrecompileInput {
            data,
            gas,
            caller: Address::ZERO,
            value: U256::ZERO,
            target_address: address,
            bytecode_address: address,
            internals: EvmInternals::new(&mut ctx.journaled_state, &ctx.block),
        })
    }

    /// The previous ModExp-only override, kept to prove the MRB wrapper charges the same.
    fn legacy_modexp(input: &[u8], gas_limit: u64) -> PrecompileResult {
        fn gas(base_len: u64, exp_len: u64, mod_len: u64, exp_highp: &U256) -> u64 {
            modexp::osaka_gas_calc(base_len, exp_len, mod_len, exp_highp).saturating_mul(10)
        }
        modexp::run_inner::<_, true>(input, gas_limit, gas(0, 0, 0, &U256::ZERO), gas)
    }

    fn modexp_input(base_len: usize, exp: &[u8], mod_len: usize) -> Vec<u8> {
        let mut input = Vec::new();
        for len in [base_len, exp.len(), mod_len] {
            input.extend_from_slice(&U256::from(len).to_be_bytes::<32>());
        }
        input.extend(std::iter::repeat_n(0xab, base_len));
        input.extend_from_slice(exp);
        input.extend(std::iter::repeat_n(0xcd, mod_len));
        input
    }

    #[test]
    fn v1_keeps_cancun_precompiles_pure() {
        let precompiles = hl_precompiles(HlSpecId::V1);
        assert!(precompiles.get(secp256k1::ECRECOVER.address()).unwrap().is_pure());
    }

    #[test]
    fn v2_mrb_precompiles_remain_cacheable_and_keep_their_ids() {
        let v1 = hl_precompiles(HlSpecId::V1);
        let v2 = hl_precompiles(HlSpecId::V2);
        for address in mrb::affected_precompile_addresses() {
            let precompile = v2.get(&address).unwrap();
            assert!(precompile.is_pure());
            assert_eq!(precompile.precompile_id(), v1.get(&address).unwrap().precompile_id());
            assert!(!matches!(precompile.precompile_id(), PrecompileId::Custom(_)));
        }
    }

    #[test]
    fn v2_modexp_matches_the_legacy_override() {
        let address = *modexp::BERLIN.address();
        let inputs = [
            Vec::new(),
            modexp_input(0, &[], 0),
            modexp_input(1, &[3], 1),
            modexp_input(32, &[0xff; 32], 32),
            modexp_input(64, &[0x01; 33], 64),
            modexp_input(256, &[0x80; 64], 256),
            modexp_input(1024, &[0x7f; 40], 1024),
            modexp_input(1025, &[1], 1),
        ];
        for input in &inputs {
            let price = legacy_modexp(input, u64::MAX).map(|output| output.gas_used).ok();
            let mut gas_limits = vec![0, 4_999, 5_000, 5_001, 1_000_000, u64::MAX];
            if let Some(price) = price {
                gas_limits.extend([price - 1, price, price + 1, price + 9]);
            }
            for gas in gas_limits {
                let legacy = legacy_modexp(input, gas);
                let mrb = call(HlSpecId::V2, address, input, gas);
                assert_eq!(legacy, mrb, "input length {}, gas {gas}", input.len());
            }
        }
    }

    #[test]
    fn v2_charges_multiplied_prices_with_an_exact_out_of_gas_boundary() {
        let ecrecover = *secp256k1::ECRECOVER.address();
        let input = hex!(
            "456e9aea5e197a1f1af7a3e85a3212fa4049a3ba34c2289b4c860fc0b0c64ef3000000000000000000000000000000000000000000000000000000000000001c9242685bf161793cc25603c231bc2f568eb630ea16aa137d2664ac80388256084f8ae3bd7535248d0bd448298cc2e2071e56992d0774dc340c368ae950852ada"
        );
        assert_eq!(call(HlSpecId::V1, ecrecover, &input, 3_000).unwrap().gas_used, 3_000);
        assert_eq!(call(HlSpecId::V2, ecrecover, &input, 15_000).unwrap().gas_used, 15_000);
        assert_eq!(call(HlSpecId::V2, ecrecover, &input, 14_999), Err(PrecompileError::OutOfGas));
        assert_eq!(
            call(HlSpecId::V2, ecrecover, &input, 15_000).unwrap().bytes,
            call(HlSpecId::V1, ecrecover, &input, 3_000).unwrap().bytes
        );

        let pairing = bn254::pair::ADDRESS;
        assert_eq!(call(HlSpecId::V1, pairing, &[], 45_000).unwrap().gas_used, 45_000);
        assert_eq!(call(HlSpecId::V2, pairing, &[], 450_000).unwrap().gas_used, 450_000);
        assert_eq!(call(HlSpecId::V2, pairing, &[], 449_999), Err(PrecompileError::OutOfGas));
    }
}
