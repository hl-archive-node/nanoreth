//! Block source that fetches blocks from a trusted nanoreth node via RPC.
//!
//! Uses debug RPC endpoints to get raw block data including system transactions:
//! - `debug_getRawBlock` - get RLP-encoded block with all transactions (including system txs)
//! - `debug_getRawReceipts` - get RLP-encoded receipts
//! - `eth_blockPrecompileData` - get HlExtras (read_precompile_calls, highest_precompile_address)

use super::BlockSource;
use crate::node::types::{BlockAndReceipts, HlExtras, LegacyReceipt};
use crate::HlBlock;
use alloy_primitives::U256;
use alloy_rlp::Decodable;
use eyre::Context;
use futures::{future::BoxFuture, FutureExt};
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use jsonrpsee_core::client::ClientT;
use jsonrpsee_core::params::BatchRequestBuilder;
use reth_metrics::{metrics, Metrics};
use serde_json::value::RawValue;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Block source that fetches blocks from a trusted nanoreth node via RPC
#[derive(Debug, Clone)]
pub struct RpcBlockSource {
    client: HttpClient,
    polling_interval: Duration,
    metrics: RpcBlockSourceMetrics,
}

#[derive(Metrics, Clone)]
#[metrics(scope = "block_source.rpc")]
pub struct RpcBlockSourceMetrics {
    /// How many times the RPC block source is polling for a block
    pub polling_attempt: metrics::Counter,
    /// How many times the RPC block source has fetched a block
    pub fetched: metrics::Counter,
    /// How many RPC errors occurred
    pub rpc_errors: metrics::Counter,
}

impl RpcBlockSource {
    /// Create a new RPC block source
    pub fn new(rpc_url: impl AsRef<str>, polling_interval: Duration) -> eyre::Result<Self> {
        let client = HttpClientBuilder::default()
            .request_timeout(Duration::from_secs(30))
            .build(rpc_url.as_ref())
            .wrap_err("Failed to build HTTP client")?;

        info!(url = %rpc_url.as_ref(), "Created RPC block source");

        Ok(Self { client, polling_interval, metrics: RpcBlockSourceMetrics::default() })
    }

    /// Fetch a single block with all its data using batched RPC calls
    async fn fetch_block(&self, height: u64) -> eyre::Result<BlockAndReceipts> {
        self.metrics.polling_attempt.increment(1);

        let block_id = format!("0x{:x}", height);

        // Build batch request using debug endpoints for raw data (includes system txs)
        let mut batch = BatchRequestBuilder::new();
        batch
            .insert("debug_getRawBlock", vec![serde_json::json!(block_id)])
            .wrap_err("Failed to add debug_getRawBlock to batch")?;
        batch
            .insert("debug_getRawReceipts", vec![serde_json::json!(block_id)])
            .wrap_err("Failed to add debug_getRawReceipts to batch")?;
        batch
            .insert("eth_blockPrecompileData", vec![serde_json::json!(block_id)])
            .wrap_err("Failed to add eth_blockPrecompileData to batch")?;

        let responses: jsonrpsee::core::client::BatchResponse<'_, Box<RawValue>> = self
            .client
            .batch_request(batch)
            .await
            .wrap_err("Batch RPC request failed")?;

        let mut responses_iter = responses.into_iter();

        // Parse raw block response (RLP-encoded HlBlock including system txs)
        let block_raw = responses_iter
            .next()
            .ok_or_else(|| eyre::eyre!("Missing block response"))?
            .wrap_err("Block RPC error")?;
        let block_hex: String = serde_json::from_str(block_raw.get())
            .wrap_err("Failed to parse block hex response")?;
        let block = decode_raw_block(&block_hex)
            .wrap_err_with(|| format!("Failed to decode raw block at height {}", height))?;

        info!(height, tx_count = block.body.inner.transactions.len(), "Decoded raw block");

        // Parse raw receipts response
        let receipts_raw = responses_iter
            .next()
            .ok_or_else(|| eyre::eyre!("Missing receipts response"))?
            .wrap_err("Receipts RPC error")?;
        let receipts_hex: Vec<String> = serde_json::from_str(receipts_raw.get())
            .wrap_err("Failed to parse receipts hex response")?;
        let receipts = decode_raw_receipts(&receipts_hex)
            .wrap_err_with(|| format!("Failed to decode raw receipts at height {}", height))?;

        // Parse extras response (may not exist on all nodes)
        let extras_raw = responses_iter
            .next()
            .ok_or_else(|| eyre::eyre!("Missing extras response"))?
            .wrap_err("Extras RPC error")?;
        let extras: HlExtras = serde_json::from_str(extras_raw.get()).unwrap_or_default();

        // Convert to BlockAndReceipts
        let block_and_receipts = convert_to_block_and_receipts(block, receipts, extras)?;

        self.metrics.fetched.increment(1);
        debug!(height, "Fetched block via RPC");

        Ok(block_and_receipts)
    }
}

impl BlockSource for RpcBlockSource {
    fn collect_block(&self, height: u64) -> BoxFuture<'static, eyre::Result<BlockAndReceipts>> {
        let this = self.clone();
        async move { this.fetch_block(height).await }.boxed()
    }

    fn find_latest_block_number(&self) -> BoxFuture<'static, Option<u64>> {
        let client = self.client.clone();
        async move {
            let block_number: Result<U256, _> =
                client.request("eth_blockNumber", Vec::<()>::new()).await;
            match block_number {
                Ok(num) => {
                    let tip = num.to::<u64>();
                    info!("Latest block number from RPC: {} (starting sync from block 1)", tip);
                    // For RPC source, we always start from block 1 for initial sync
                    // The poller will sync sequentially from 1 to the tip
                    Some(1)
                }
                Err(e) => {
                    warn!("Failed to get latest block number: {}", e);
                    None
                }
            }
        }
        .boxed()
    }

    fn recommended_chunk_size(&self) -> u64 {
        100
    }

    fn polling_interval(&self) -> Duration {
        self.polling_interval
    }
}

/// Decode hex string to bytes
fn hex_decode(s: &str) -> eyre::Result<Vec<u8>> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let mut bytes = Vec::with_capacity(s.len() / 2);
    for i in (0..s.len()).step_by(2) {
        let byte = u8::from_str_radix(&s[i..i + 2], 16)
            .wrap_err_with(|| format!("Invalid hex at position {}", i))?;
        bytes.push(byte);
    }
    Ok(bytes)
}

/// Decode raw RLP-encoded block from hex string
/// debug_getRawBlock returns standard Ethereum block RLP, not custom HlBlock format
fn decode_raw_block(hex: &str) -> eyre::Result<HlBlock> {
    use crate::node::primitives::{HlBlockBody, HlHeader, TransactionSigned};
    use alloy_consensus::BlockBody;
    use reth_primitives::Block as RethBlock;

    let bytes = hex_decode(hex)?;

    // Try decoding as HlBlock first (custom format with trailing fields)
    if let Ok(block) = HlBlock::decode(&mut bytes.as_slice()) {
        if !block.body.inner.transactions.is_empty() || block.body.read_precompile_calls.is_some() {
            return Ok(block);
        }
    }

    // Fall back to standard Ethereum block format
    // This is what debug_getRawBlock returns from reth
    let reth_block: reth_primitives::Block = RethBlock::decode(&mut bytes.as_slice())
        .map_err(|e| eyre::eyre!("RLP decode error (standard format): {}", e))?;

    // Convert reth types to our types
    let header = HlHeader::from(reth_block.header);
    let transactions: Vec<TransactionSigned> = reth_block
        .body
        .transactions
        .into_iter()
        .map(|tx| convert_reth_tx_to_hl(tx))
        .collect();
    let ommers: Vec<HlHeader> = reth_block
        .body
        .ommers
        .into_iter()
        .map(|h| HlHeader::from(h))
        .collect();

    Ok(HlBlock {
        header,
        body: HlBlockBody {
            inner: BlockBody {
                transactions,
                ommers,
                withdrawals: reth_block.body.withdrawals,
            },
            sidecars: None,
            read_precompile_calls: None,
            highest_precompile_address: None,
        },
    })
}

/// Convert reth_primitives::TransactionSigned to node::primitives::TransactionSigned
fn convert_reth_tx_to_hl(
    tx: reth_primitives::TransactionSigned,
) -> crate::node::primitives::TransactionSigned {
    use alloy_consensus::{Signed, TxEip4844Variant, TypedTransaction};

    // Extract signature and transaction data based on type
    match tx {
        reth_primitives::TransactionSigned::Legacy(signed) => {
            crate::node::primitives::TransactionSigned::from(Signed::new_unhashed(
                signed.tx().clone(),
                signed.signature().clone(),
            ))
        }
        reth_primitives::TransactionSigned::Eip2930(signed) => {
            crate::node::primitives::TransactionSigned::from(Signed::new_unhashed(
                signed.tx().clone(),
                signed.signature().clone(),
            ))
        }
        reth_primitives::TransactionSigned::Eip1559(signed) => {
            crate::node::primitives::TransactionSigned::from(Signed::new_unhashed(
                signed.tx().clone(),
                signed.signature().clone(),
            ))
        }
        reth_primitives::TransactionSigned::Eip4844(signed) => {
            // For EIP-4844, signed.tx() returns &TxEip4844
            // We need to wrap it in TxEip4844Variant to create a TypedTransaction
            let eip4844_variant = TxEip4844Variant::TxEip4844(signed.tx().clone());
            let typed_tx = TypedTransaction::Eip4844(eip4844_variant);
            crate::node::primitives::TransactionSigned::from(Signed::new_unhashed(
                typed_tx,
                signed.signature().clone(),
            ))
        }
        reth_primitives::TransactionSigned::Eip7702(signed) => {
            crate::node::primitives::TransactionSigned::from(Signed::new_unhashed(
                signed.tx().clone(),
                signed.signature().clone(),
            ))
        }
    }
}

/// Decode raw RLP-encoded receipts from hex strings
fn decode_raw_receipts(hex_receipts: &[String]) -> eyre::Result<Vec<LegacyReceipt>> {
    use alloy_consensus::{ReceiptEnvelope, TxReceipt};

    let mut receipts = Vec::with_capacity(hex_receipts.len());

    for (idx, hex) in hex_receipts.iter().enumerate() {
        let bytes = hex_decode(hex)
            .wrap_err_with(|| format!("Failed to decode receipt hex at index {}", idx))?;

        // Receipts are encoded with EIP-2718 envelope
        let envelope = ReceiptEnvelope::decode(&mut bytes.as_slice())
            .map_err(|e| eyre::eyre!("RLP decode error for receipt {}: {}", idx, e))?;

        // Extract receipt data from envelope
        let (tx_type, status, cumulative_gas_used, logs) = match &envelope {
            ReceiptEnvelope::Legacy(r) => {
                (0u8, r.status(), r.cumulative_gas_used(), r.logs().to_vec())
            }
            ReceiptEnvelope::Eip2930(r) => {
                (1u8, r.status(), r.cumulative_gas_used(), r.logs().to_vec())
            }
            ReceiptEnvelope::Eip1559(r) => {
                (2u8, r.status(), r.cumulative_gas_used(), r.logs().to_vec())
            }
            ReceiptEnvelope::Eip4844(r) => {
                (3u8, r.status(), r.cumulative_gas_used(), r.logs().to_vec())
            }
            ReceiptEnvelope::Eip7702(r) => {
                (4u8, r.status(), r.cumulative_gas_used(), r.logs().to_vec())
            }
        };

        receipts.push(LegacyReceipt::new(tx_type, status, cumulative_gas_used, logs));
    }

    Ok(receipts)
}

/// Convert decoded HlBlock and receipts to BlockAndReceipts format
fn convert_to_block_and_receipts(
    block: HlBlock,
    receipts: Vec<LegacyReceipt>,
    extras: HlExtras,
) -> eyre::Result<BlockAndReceipts> {
    use crate::node::types::reth_compat::{SealedBlock, SealedHeader, TransactionSigned};
    use crate::node::types::EvmBlock;
    use alloy_consensus::Header;
    use alloy_primitives::Signature;
    use reth_codecs::alloy::transaction::Envelope;

    // Convert HlHeader to alloy Header
    let header = Header {
        parent_hash: block.header.parent_hash,
        ommers_hash: block.header.ommers_hash,
        beneficiary: block.header.beneficiary,
        state_root: block.header.state_root,
        transactions_root: block.header.transactions_root,
        receipts_root: block.header.receipts_root,
        logs_bloom: block.header.logs_bloom,
        difficulty: block.header.difficulty,
        number: block.header.number,
        gas_limit: block.header.gas_limit,
        gas_used: block.header.gas_used,
        timestamp: block.header.timestamp,
        extra_data: block.header.extra_data.clone(),
        mix_hash: block.header.mix_hash,
        nonce: block.header.nonce,
        base_fee_per_gas: block.header.base_fee_per_gas,
        withdrawals_root: block.header.withdrawals_root,
        blob_gas_used: block.header.blob_gas_used,
        excess_blob_gas: block.header.excess_blob_gas,
        parent_beacon_block_root: block.header.parent_beacon_block_root,
        requests_hash: block.header.requests_hash,
    };

    let hash = block.header.hash_slow();

    // Convert transactions from HlBlock's TransactionSigned to internal format
    // HlBlock.body.inner.transactions contains node::primitives::TransactionSigned
    let transactions: Vec<TransactionSigned> = block
        .body
        .inner
        .transactions
        .into_iter()
        .map(|tx| {
            // Get signature from the transaction
            let sig = tx.signature();
            let signature = Signature::new(sig.r(), sig.s(), sig.v());

            // Get the inner reth TransactionSigned and extract the transaction
            let inner = tx.into_inner();
            let transaction = match &inner {
                reth_primitives::TransactionSigned::Legacy(signed) => {
                    crate::node::types::reth_compat::Transaction::Legacy(signed.tx().clone())
                }
                reth_primitives::TransactionSigned::Eip2930(signed) => {
                    crate::node::types::reth_compat::Transaction::Eip2930(signed.tx().clone())
                }
                reth_primitives::TransactionSigned::Eip1559(signed) => {
                    crate::node::types::reth_compat::Transaction::Eip1559(signed.tx().clone())
                }
                reth_primitives::TransactionSigned::Eip4844(signed) => {
                    crate::node::types::reth_compat::Transaction::Eip4844(signed.tx().clone())
                }
                reth_primitives::TransactionSigned::Eip7702(signed) => {
                    crate::node::types::reth_compat::Transaction::Eip7702(signed.tx().clone())
                }
            };

            TransactionSigned { signature, transaction }
        })
        .collect();

    // Build the SealedBlock
    let sealed_block = SealedBlock {
        header: SealedHeader { hash, header },
        body: alloy_consensus::BlockBody {
            transactions,
            ommers: vec![],
            withdrawals: block.body.inner.withdrawals,
        },
    };

    // Use extras from RPC, falling back to block body data
    let read_precompile_calls = extras
        .read_precompile_calls
        .or(block.body.read_precompile_calls)
        .unwrap_or_default();
    let highest_precompile_address =
        extras.highest_precompile_address.or(block.body.highest_precompile_address);

    Ok(BlockAndReceipts {
        block: EvmBlock::Reth115(sealed_block),
        receipts,
        system_txs: vec![], // System txs are already included in the raw block
        read_precompile_calls,
        highest_precompile_address,
    })
}
