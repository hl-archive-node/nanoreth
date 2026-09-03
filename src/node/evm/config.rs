use super::ScopedReadPrecompileForwarder;
use super::{executor::HlBlockExecutor, factory::HlEvmFactory};
use crate::{
    HlBlock, HlBlockBody, HlHeader, HlPrimitives,
    chainspec::HlChainSpec,
    evm::{spec::HlSpecId, transaction::HlTxEnv},
    hardforks::HlHardforks,
    node::{
        evm::{executor::is_system_transaction, receipt_builder::RethReceiptBuilder},
        primitives::{BlockBody, TransactionSigned},
        rpc::engine_api::validator::HlExecutionData,
        types::HlExtras,
    },
};
use alloy_consensus::{BlockHeader, EMPTY_OMMER_ROOT_HASH, Header, Transaction as _, TxReceipt};
use alloy_eips::{Encodable2718, merge::BEACON_NONCE};
use alloy_primitives::{Log, U256};
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use reth_evm::{
    ConfigureEngineEvm, ConfigureEvm, EvmEnv, EvmEnvFor, EvmFactory, ExecutableTxIterator,
    ExecutionCtxFor, FromRecoveredTx, FromTxWithEncoded, IntoTxEnv, NextBlockEnvAttributes,
    block::{BlockExecutionError, BlockExecutorFactory, BlockExecutorFor},
    eth::{EthBlockExecutionCtx, receipt_builder::ReceiptBuilder},
    execute::{BlockAssembler, BlockAssemblerInput},
    precompiles::PrecompilesMap,
};
use reth_evm_ethereum::EthBlockAssembler;
use reth_payload_primitives::NewPayloadError;
use reth_primitives::{BlockTy, HeaderTy, Receipt, SealedBlock, SealedHeader, logs_bloom};
use reth_primitives_traits::{SignerRecoverable, WithEncoded, proofs};
use reth_provider::BlockExecutionResult;
use reth_revm::State;
use revm::{
    Inspector,
    context::{BlockEnv, CfgEnv, TxEnv},
    context_interface::block::BlobExcessGasAndPrice,
    primitives::hardfork::SpecId,
};
use std::{borrow::Cow, convert::Infallible, sync::Arc};

#[derive(Debug, Clone)]
pub struct HlBlockAssembler {
    inner: EthBlockAssembler<HlChainSpec>,
}

impl<F> BlockAssembler<F> for HlBlockAssembler
where
    F: for<'a> BlockExecutorFactory<
            ExecutionCtx<'a> = HlBlockExecutionCtx<'a>,
            Transaction = TransactionSigned,
            Receipt = Receipt,
        >,
{
    type Block = HlBlock;

    fn assemble_block(
        &self,
        input: BlockAssemblerInput<'_, '_, F, HlHeader>,
    ) -> Result<Self::Block, BlockExecutionError> {
        // TODO: Copy of EthBlockAssembler::assemble_block
        let inner = &self.inner;
        let BlockAssemblerInput {
            evm_env,
            execution_ctx: ctx,
            parent,
            transactions,
            output: BlockExecutionResult { receipts, requests, gas_used },
            state_root,
            ..
        } = input;

        let timestamp = evm_env.block_env.timestamp.saturating_to();

        // Filter out system tx receipts
        let transactions_for_root: Vec<_> =
            transactions.iter().filter(|t| !is_system_transaction(t)).cloned().collect();
        let receipts_for_root: Vec<_> =
            receipts.iter().filter(|r| r.cumulative_gas_used() != 0).cloned().collect();

        let transactions_root = proofs::calculate_transaction_root(&transactions_for_root);
        let receipts_root = Receipt::calculate_receipt_root_no_memo(&receipts_for_root);
        let logs_bloom = logs_bloom(receipts.iter().flat_map(|r| r.logs()));

        let withdrawals = inner
            .chain_spec
            .is_shanghai_active_at_timestamp(timestamp)
            .then(|| ctx.ctx.withdrawals.map(|w| w.into_owned()).unwrap_or_default());

        let withdrawals_root =
            withdrawals.as_deref().map(|w| proofs::calculate_withdrawals_root(w));
        let requests_hash = inner
            .chain_spec
            .is_prague_active_at_timestamp(timestamp)
            .then(|| requests.requests_hash());

        let mut excess_blob_gas = None;
        let mut blob_gas_used = None;

        // only determine cancun fields when active
        if inner.chain_spec.is_cancun_active_at_timestamp(timestamp) {
            blob_gas_used =
                Some(transactions.iter().map(|tx| tx.blob_gas_used().unwrap_or_default()).sum());
            excess_blob_gas = if inner.chain_spec.is_cancun_active_at_timestamp(parent.timestamp) {
                parent.maybe_next_block_excess_blob_gas(
                    inner.chain_spec.blob_params_at_timestamp(timestamp),
                )
            } else {
                // for the first post-fork block, both parent.blob_gas_used and
                // parent.excess_blob_gas are evaluated as 0
                Some(
                    alloy_eips::eip7840::BlobParams::cancun()
                        .next_block_excess_blob_gas_osaka(0, 0, 0),
                )
            };
        }

        let header = Header {
            parent_hash: ctx.ctx.parent_hash,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: evm_env.block_env.beneficiary,
            state_root,
            transactions_root,
            receipts_root,
            withdrawals_root,
            logs_bloom,
            timestamp,
            mix_hash: evm_env.block_env.prevrandao.unwrap_or_default(),
            nonce: BEACON_NONCE.into(),
            base_fee_per_gas: Some(evm_env.block_env.basefee),
            number: evm_env.block_env.number.saturating_to(),
            gas_limit: evm_env.block_env.gas_limit,
            difficulty: evm_env.block_env.difficulty,
            gas_used: *gas_used,
            extra_data: inner.extra_data.clone(),
            parent_beacon_block_root: ctx.ctx.parent_beacon_block_root,
            blob_gas_used,
            excess_blob_gas,
            requests_hash,
        };
        let system_tx_count =
            transactions.iter().filter(|t| is_system_transaction(t)).count() as u64;
        let header = HlHeader::from_ethereum_header(header, receipts, system_tx_count);

        Ok(Self::Block {
            header,
            body: HlBlockBody {
                inner: BlockBody { transactions, ommers: Default::default(), withdrawals },
                sidecars: None,
                read_precompile_calls: ctx.extras.read_precompile_calls.clone(),
                highest_precompile_address: ctx.extras.highest_precompile_address,
            },
        })
    }
}

impl HlBlockAssembler {
    pub fn new(chain_spec: Arc<HlChainSpec>) -> Self {
        Self { inner: EthBlockAssembler::new(chain_spec) }
    }
}

/// Ethereum-related EVM configuration.
#[derive(Debug, Clone)]
pub struct HlEvmConfig {
    /// Inner [`HlBlockExecutorFactory`].
    pub executor_factory:
        HlBlockExecutorFactory<RethReceiptBuilder, Arc<HlChainSpec>, HlEvmFactory>,
    /// Ethereum block assembler.
    pub block_assembler: HlBlockAssembler,
}

impl HlEvmConfig {
    /// Creates a new Ethereum EVM configuration with the given chain spec.
    pub fn new(chain_spec: Arc<HlChainSpec>) -> Self {
        Self::hl(chain_spec)
    }

    /// Creates a new Ethereum EVM configuration.
    pub fn hl(chain_spec: Arc<HlChainSpec>) -> Self {
        Self::new_with_evm_factory(chain_spec, HlEvmFactory::default())
    }
}

impl HlEvmConfig {
    /// Creates a new Ethereum EVM configuration with the given chain spec and EVM factory.
    pub fn new_with_evm_factory(chain_spec: Arc<HlChainSpec>, evm_factory: HlEvmFactory) -> Self {
        Self {
            block_assembler: HlBlockAssembler::new(chain_spec.clone()),
            executor_factory: HlBlockExecutorFactory::new(
                RethReceiptBuilder::default(),
                chain_spec,
                evm_factory,
            ),
        }
    }

    /// Returns the chain spec associated with this configuration.
    pub const fn chain_spec(&self) -> &Arc<HlChainSpec> {
        self.executor_factory.spec()
    }
}

/// Ethereum block executor factory.
#[derive(Debug, Clone, Default, Copy)]
pub struct HlBlockExecutorFactory<
    R = RethReceiptBuilder,
    Spec = Arc<HlChainSpec>,
    EvmFactory = HlEvmFactory,
> {
    /// Receipt builder.
    receipt_builder: R,
    /// Chain specification.
    spec: Spec,
    /// EVM factory.
    evm_factory: EvmFactory,
}

impl<R, Spec, EvmFactory> HlBlockExecutorFactory<R, Spec, EvmFactory> {
    /// Creates a new [`HlBlockExecutorFactory`] with the given spec, [`EvmFactory`], and
    /// [`ReceiptBuilder`].
    pub const fn new(receipt_builder: R, spec: Spec, evm_factory: EvmFactory) -> Self {
        Self { receipt_builder, spec, evm_factory }
    }

    /// Exposes the receipt builder.
    pub const fn receipt_builder(&self) -> &R {
        &self.receipt_builder
    }

    /// Exposes the chain specification.
    pub const fn spec(&self) -> &Spec {
        &self.spec
    }
}

#[derive(Debug, Clone)]
pub struct HlBlockExecutionCtx<'a> {
    ctx: EthBlockExecutionCtx<'a>,
    pub extras: HlExtras,
    /// Resolves read precompile inputs that `extras` did not record.
    ///
    /// Only the RPC simulation path sets this, and only for blocks that were never mined.
    /// Executing a real block must leave it `None`: there the recorded calls are consensus data,
    /// and an input missing from them means the block itself is wrong.
    read_precompile_forwarder: Option<ScopedReadPrecompileForwarder>,
}

impl HlBlockExecutionCtx<'_> {
    pub(crate) fn enable_rpc_read_precompile_forwarding(
        &mut self,
        forwarder: Option<ScopedReadPrecompileForwarder>,
    ) {
        self.read_precompile_forwarder = forwarder;
    }

    pub(super) fn read_precompile_forwarder(&self) -> Option<ScopedReadPrecompileForwarder> {
        self.read_precompile_forwarder.clone()
    }
}

impl<R, Spec, EvmF> BlockExecutorFactory for HlBlockExecutorFactory<R, Spec, EvmF>
where
    R: ReceiptBuilder<Transaction = TransactionSigned, Receipt: TxReceipt<Log = Log>>,
    Spec: EthereumHardforks + HlHardforks + EthChainSpec + Hardforks + Clone,
    EvmF: EvmFactory<
            Tx: FromRecoveredTx<TransactionSigned> + FromTxWithEncoded<TransactionSigned>,
            Precompiles = PrecompilesMap,
        >,
    R::Transaction: From<TransactionSigned> + Clone,
    Self: 'static,
    HlTxEnv<TxEnv>: IntoTxEnv<<EvmF as EvmFactory>::Tx>,
{
    type EvmFactory = EvmF;
    type ExecutionCtx<'a> = HlBlockExecutionCtx<'a>;
    type Transaction = TransactionSigned;
    type Receipt = R::Receipt;

    fn evm_factory(&self) -> &Self::EvmFactory {
        &self.evm_factory
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        evm: <Self::EvmFactory as EvmFactory>::Evm<&'a mut State<DB>, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> impl BlockExecutorFor<'a, Self, DB, I>
    where
        DB: alloy_evm::Database + 'a,
        I: Inspector<<Self::EvmFactory as EvmFactory>::Context<&'a mut State<DB>>> + 'a,
    {
        HlBlockExecutor::new(evm, ctx, self.spec().clone(), self.receipt_builder())
    }
}

static EMPTY_OMMERS: [Header; 0] = [];

impl ConfigureEvm for HlEvmConfig
where
    Self: Send + Sync + Unpin + Clone + 'static,
{
    type Primitives = HlPrimitives;
    type Error = Infallible;
    type NextBlockEnvCtx = NextBlockEnvAttributes;
    type BlockExecutorFactory = HlBlockExecutorFactory;
    type BlockAssembler = Self;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        &self.executor_factory
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        self
    }

    fn evm_env(&self, header: &HlHeader) -> Result<EvmEnv<HlSpecId>, Self::Error> {
        let blob_params = self.chain_spec().blob_params_at_timestamp(header.timestamp);
        let spec = revm_spec_by_timestamp_and_block_number(
            self.chain_spec().clone(),
            header.timestamp(),
            header.number(),
        );

        // configure evm env based on parent block
        let mut cfg_env =
            CfgEnv::new().with_chain_id(self.chain_spec().chain().id()).with_spec(spec);
        if let Some(blob_params) = &blob_params {
            cfg_env.set_max_blobs_per_tx(blob_params.max_blobs_per_tx);
        }

        // TODO: enable only for system transactions
        cfg_env.disable_base_fee = true;
        cfg_env.disable_eip3607 = true;

        // derive the EIP-4844 blob fees from the header's `excess_blob_gas` and the current
        // blobparams
        let blob_excess_gas_and_price =
            header.excess_blob_gas.zip(blob_params).map(|(excess_blob_gas, params)| {
                let blob_gasprice = params.calc_blob_fee(excess_blob_gas);
                BlobExcessGasAndPrice { excess_blob_gas, blob_gasprice }
            });

        let eth_spec = spec.into_eth_spec();

        let block_env = BlockEnv {
            number: U256::from(header.number()),
            beneficiary: header.beneficiary(),
            timestamp: U256::from(header.timestamp()),
            difficulty: if eth_spec >= SpecId::MERGE { U256::ZERO } else { header.difficulty() },
            prevrandao: if eth_spec >= SpecId::MERGE { header.mix_hash() } else { None },
            gas_limit: header.gas_limit(),
            basefee: header.base_fee_per_gas().unwrap_or_default(),
            blob_excess_gas_and_price,
        };

        Ok(EvmEnv { cfg_env, block_env })
    }

    fn next_evm_env(
        &self,
        parent: &HlHeader,
        attributes: &Self::NextBlockEnvCtx,
    ) -> Result<EvmEnv<HlSpecId>, Self::Error> {
        // ensure we're not missing any timestamp based hardforks
        let spec_id = revm_spec_by_timestamp_and_block_number(
            self.chain_spec().clone(),
            attributes.timestamp,
            parent.number() + 1,
        );

        // configure evm env based on parent block
        let cfg_env =
            CfgEnv::new().with_chain_id(self.chain_spec().chain().id()).with_spec(spec_id);

        // if the parent block did not have excess blob gas (i.e. it was pre-cancun), but it is
        // cancun now, we need to set the excess blob gas to the default value(0)
        let blob_excess_gas_and_price = spec_id
            .into_eth_spec()
            .is_enabled_in(SpecId::CANCUN)
            .then_some(BlobExcessGasAndPrice { excess_blob_gas: 0, blob_gasprice: 0 });

        let basefee = parent.next_block_base_fee(
            self.chain_spec().base_fee_params_at_timestamp(attributes.timestamp),
        );

        let block_env = BlockEnv {
            number: U256::from(parent.number() + 1),
            beneficiary: attributes.suggested_fee_recipient,
            timestamp: U256::from(attributes.timestamp),
            difficulty: U256::ZERO,
            prevrandao: Some(attributes.prev_randao),
            gas_limit: attributes.gas_limit,
            // calculate basefee based on parent block's gas usage
            basefee: basefee.unwrap_or_default(),
            // calculate excess gas based on parent block's blob gas usage
            blob_excess_gas_and_price,
        };

        Ok(EvmEnv { cfg_env, block_env })
    }

    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<BlockTy<Self::Primitives>>,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        let block_body = block.body();
        Ok(HlBlockExecutionCtx {
            ctx: EthBlockExecutionCtx {
                parent_hash: block.header().parent_hash,
                parent_beacon_block_root: block.header().parent_beacon_block_root,
                ommers: &EMPTY_OMMERS,
                withdrawals: block.body().withdrawals.as_ref().map(Cow::Borrowed),
            },
            extras: HlExtras {
                read_precompile_calls: block_body.read_precompile_calls.clone(),
                highest_precompile_address: block_body.highest_precompile_address,
            },
            read_precompile_forwarder: None,
        })
    }

    fn context_for_next_block(
        &self,
        parent: &SealedHeader<HeaderTy<Self::Primitives>>,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<ExecutionCtxFor<'_, Self>, Self::Error> {
        Ok(HlBlockExecutionCtx {
            ctx: EthBlockExecutionCtx {
                parent_hash: parent.hash(),
                parent_beacon_block_root: attributes.parent_beacon_block_root,
                ommers: &[],
                withdrawals: attributes.withdrawals.map(Cow::Owned),
            },
            // A block that has not been mined has no recorded calls of its own. The RPC
            // simulation path overwrites this with the head's precompile address range and a
            // forwarder; see `HlEthApi::simulate_v1`.
            extras: HlExtras::default(),
            read_precompile_forwarder: None,
        })
    }
}

impl ConfigureEngineEvm<HlExecutionData> for HlEvmConfig {
    fn evm_env_for_payload(&self, payload: &HlExecutionData) -> EvmEnvFor<Self> {
        self.evm_env(&payload.0.header).unwrap()
    }

    fn context_for_payload<'a>(&self, payload: &'a HlExecutionData) -> ExecutionCtxFor<'a, Self> {
        let block = &payload.0;
        HlBlockExecutionCtx {
            ctx: EthBlockExecutionCtx {
                parent_hash: block.header.parent_hash,
                parent_beacon_block_root: block.header.parent_beacon_block_root,
                ommers: &EMPTY_OMMERS,
                withdrawals: block.body.withdrawals.as_ref().map(Cow::Borrowed),
            },
            extras: HlExtras {
                read_precompile_calls: block.body.read_precompile_calls.clone(),
                highest_precompile_address: block.body.highest_precompile_address,
            },
            read_precompile_forwarder: None,
        }
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &HlExecutionData,
    ) -> impl ExecutableTxIterator<Self> {
        payload.0.body.transactions.clone().into_iter().map(move |tx| {
            let recovered = tx.try_into_recovered().map_err(NewPayloadError::other)?;
            Ok::<_, NewPayloadError>(WithEncoded::new(recovered.encoded_2718().into(), recovered))
        })
    }
}

/// Map the latest active hardfork at the given timestamp or block number to a [`HlSpecId`].
pub fn revm_spec_by_timestamp_and_block_number(
    _chain_spec: impl HlHardforks,
    _timestamp: u64,
    _block_number: u64,
) -> HlSpecId {
    HlSpecId::V1
}
