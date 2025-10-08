use crate::{
    node::{
        primitives::tx_wrapper::{convert_to_eth_block_body, convert_to_hl_block_body},
        types::HlExtras,
    },
    HlBlock, HlBlockBody, HlPrimitives,
};
use alloy_consensus::BlockHeader;
use alloy_primitives::Bytes;
use reth_chainspec::EthereumHardforks;
use reth_db::{
    cursor::{DbCursorRO, DbCursorRW},
    transaction::{DbTx, DbTxMut},
    DbTxUnwindExt,
};
use reth_provider::{
    providers::{ChainStorage, NodeTypesForProvider},
    BlockBodyReader, BlockBodyWriter, ChainSpecProvider, ChainStorageReader, ChainStorageWriter,
    DBProvider, DatabaseProvider, EthStorage, ProviderResult, ReadBodyInput, StorageLocation,
};

pub mod tables;

#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct HlStorage(EthStorage);

impl HlStorage {
    fn write_precompile_calls<Provider>(
        &self,
        provider: &Provider,
        inputs: Vec<(u64, HlExtras)>,
    ) -> ProviderResult<()>
    where
        Provider: DBProvider<Tx: DbTxMut>,
    {
        let mut precompile_calls_cursor: <<Provider as DBProvider>::Tx as DbTxMut>::CursorMut<
            tables::BlockReadPrecompileCalls,
        > = provider.tx_ref().cursor_write::<tables::BlockReadPrecompileCalls>()?;

        for (block_number, extras) in inputs {
            precompile_calls_cursor.append(
                block_number,
                &Bytes::copy_from_slice(
                    &rmp_serde::to_vec(&extras).expect("Failed to serialize read precompile calls"),
                ),
            )?;
        }

        Ok(())
    }

    fn read_precompile_calls<Provider>(
        &self,
        provider: &Provider,
        inputs: &[ReadBodyInput<'_, HlBlock>],
    ) -> ProviderResult<Vec<HlExtras>>
    where
        Provider: DBProvider<Tx: DbTx>,
    {
        let mut extras: Vec<HlExtras> = Vec::with_capacity(inputs.len());
        let mut precompile_calls_cursor =
            provider.tx_ref().cursor_read::<tables::BlockReadPrecompileCalls>()?;

        for (header, _transactions) in inputs {
            let precompile_calls = precompile_calls_cursor
                .seek_exact(header.number())?
                .map(|(_, calls)| calls)
                .map(|calls| rmp_serde::from_slice(&calls).unwrap())
                .unwrap_or_default();
            extras.push(precompile_calls);
        }

        Ok(extras)
    }
}

impl<Provider> BlockBodyWriter<Provider, HlBlockBody> for HlStorage
where
    Provider: DBProvider<Tx: DbTxMut>,
{
    fn write_block_bodies(
        &self,
        provider: &Provider,
        bodies: Vec<(u64, Option<HlBlockBody>)>,
        write_to: StorageLocation,
    ) -> ProviderResult<()> {
        let mut eth_bodies = Vec::with_capacity(bodies.len());
        let mut read_precompile_calls = Vec::with_capacity(bodies.len());

        for (block_number, body) in bodies {
            match body {
                Some(HlBlockBody {
                    inner,
                    sidecars: _,
                    read_precompile_calls: rpc,
                    highest_precompile_address,
                }) => {
                    eth_bodies.push((block_number, Some(convert_to_eth_block_body(inner))));
                    read_precompile_calls.push((
                        block_number,
                        HlExtras { read_precompile_calls: rpc, highest_precompile_address },
                    ));
                }
                None => {
                    eth_bodies.push((block_number, None));
                    read_precompile_calls.push((
                        block_number,
                        HlExtras {
                            read_precompile_calls: Default::default(),
                            highest_precompile_address: None,
                        },
                    ));
                }
            }
        }

        self.0.write_block_bodies(provider, eth_bodies, write_to)?;
        self.write_precompile_calls(provider, read_precompile_calls)?;

        Ok(())
    }

    fn remove_block_bodies_above(
        &self,
        provider: &Provider,
        block: u64,
        remove_from: StorageLocation,
    ) -> ProviderResult<()> {
        self.0.remove_block_bodies_above(provider, block, remove_from)?;
        provider.tx_ref().unwind_table_by_num::<tables::BlockReadPrecompileCalls>(block)?;

        Ok(())
    }
}

impl<Provider> BlockBodyReader<Provider> for HlStorage
where
    Provider: DBProvider + ChainSpecProvider<ChainSpec: EthereumHardforks>,
{
    type Block = HlBlock;

    fn read_block_bodies(
        &self,
        provider: &Provider,
        inputs: Vec<ReadBodyInput<'_, Self::Block>>,
    ) -> ProviderResult<Vec<HlBlockBody>> {
        let read_precompile_calls = self.read_precompile_calls(provider, &inputs)?;
        let eth_bodies = self.0.read_block_bodies(
            provider,
            inputs
                .into_iter()
                .map(|(header, transactions)| {
                    (header, transactions.into_iter().map(|tx| tx.into_inner()).collect())
                })
                .collect(),
        )?;

        // NOTE: sidecars are not used in HyperEVM yet.
        Ok(eth_bodies
            .into_iter()
            .zip(read_precompile_calls)
            .map(|(inner, extra)| HlBlockBody {
                inner: convert_to_hl_block_body(inner),
                sidecars: None,
                read_precompile_calls: extra.read_precompile_calls,
                highest_precompile_address: extra.highest_precompile_address,
            })
            .collect())
    }
}

impl ChainStorage<HlPrimitives> for HlStorage {
    fn reader<TX, Types>(
        &self,
    ) -> impl ChainStorageReader<DatabaseProvider<TX, Types>, HlPrimitives>
    where
        TX: DbTx + 'static,
        Types: NodeTypesForProvider<Primitives = HlPrimitives>,
    {
        self
    }

    fn writer<TX, Types>(
        &self,
    ) -> impl ChainStorageWriter<DatabaseProvider<TX, Types>, HlPrimitives>
    where
        TX: DbTxMut + DbTx + 'static,
        Types: NodeTypesForProvider<Primitives = HlPrimitives>,
    {
        self
    }
}
