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
use std::sync::Arc;
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
        debug!(height, "Fetching block via RPC");

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
                    debug!("Latest block number from RPC: {}", tip);
                    Some(tip)
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
        1000
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
/// debug_getRawBlock returns the HlBlock format with trailing fields (sidecars, read_precompile_calls, etc.)
fn decode_raw_block(hex: &str) -> eyre::Result<HlBlock> {
    use crate::node::primitives::{HlBlockBody, HlHeader, TransactionSigned};
    use alloy_consensus::BlockBody;
    use reth_primitives::Block as RethBlock;

    let bytes = hex_decode(hex)?;

    // Try decoding as HlBlock first (custom format with trailing fields)
    // This is what debug_getRawBlock returns from nanoreth
    if let Ok(block) = HlBlock::decode(&mut bytes.as_slice()) {
        return Ok(block);
    }

    // Fall back to standard Ethereum block format
    // This is what debug_getRawBlock returns from standard reth (without HL extensions)
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
    use crate::node::types::{EvmBlock, SystemTx};
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

    // Helper to convert a primitive transaction to our format
    let convert_tx = |tx: crate::node::primitives::TransactionSigned| -> TransactionSigned {
        let sig = tx.signature();
        let signature = Signature::new(sig.r(), sig.s(), sig.v());
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
    };

    // Identify system transactions by checking gas_price == 0
    // System transactions in HlBlock format have gas_price = 0
    use alloy_consensus::Transaction as TxTrait;
    let is_system_tx = |tx: &crate::node::primitives::TransactionSigned| -> bool {
        tx.gas_price().map_or(false, |price| price == 0)
    };

    // Split transactions: system txs (gas_price == 0) vs regular txs
    let all_txs: Vec<_> = block.body.inner.transactions.into_iter().collect();
    let (system_tx_primitives, regular_tx_primitives): (Vec<_>, Vec<_>) =
        all_txs.into_iter().enumerate().partition(|(_, tx)| is_system_tx(tx));

    // Count system transactions for receipt splitting (they come first in the block)
    let system_tx_count = system_tx_primitives.len();

    // Split receipts - system tx receipts are at the beginning
    let (system_receipts, regular_receipts): (Vec<_>, Vec<_>) =
        receipts.into_iter().enumerate().partition(|(i, _)| *i < system_tx_count);

    // Convert system transactions to SystemTx format
    let system_txs: Vec<SystemTx> = system_tx_primitives
        .into_iter()
        .zip(system_receipts.into_iter())
        .map(|((_, tx), (_, receipt))| {
            let converted = convert_tx(tx);
            SystemTx { tx: converted.transaction, receipt: Some(receipt) }
        })
        .collect();

    // Convert regular transactions
    let transactions: Vec<TransactionSigned> =
        regular_tx_primitives.into_iter().map(|(_, tx)| convert_tx(tx)).collect();

    // Extract regular receipts (without index)
    let receipts: Vec<LegacyReceipt> =
        regular_receipts.into_iter().map(|(_, receipt)| receipt).collect();

    // Build the SealedBlock with only regular transactions
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
        system_txs,
        read_precompile_calls,
        highest_precompile_address,
    })
}

/// Block source that distributes requests across multiple RPC peers using round-robin
#[derive(Debug, Clone)]
pub struct MultiRpcBlockSource {
    sources: Vec<RpcBlockSource>,
    current_index: Arc<std::sync::atomic::AtomicUsize>,
    polling_interval: Duration,
    metrics: MultiRpcBlockSourceMetrics,
}

#[derive(Metrics, Clone)]
#[metrics(scope = "block_source.multi_rpc")]
pub struct MultiRpcBlockSourceMetrics {
    /// Total blocks fetched across all peers
    pub fetched: metrics::Counter,
    /// Total errors across all peers
    pub errors: metrics::Counter,
    /// Number of failover attempts
    pub failovers: metrics::Counter,
}

impl MultiRpcBlockSource {
    /// Create a new multi-RPC block source from a list of URLs
    pub fn new(
        rpc_urls: impl IntoIterator<Item = impl AsRef<str>>,
        polling_interval: Duration,
    ) -> eyre::Result<Self> {
        let sources: Vec<RpcBlockSource> = rpc_urls
            .into_iter()
            .map(|url| RpcBlockSource::new(url, polling_interval))
            .collect::<eyre::Result<Vec<_>>>()?;

        if sources.is_empty() {
            return Err(eyre::eyre!("At least one RPC URL is required"));
        }

        info!(peer_count = sources.len(), "Created multi-RPC block source");

        Ok(Self {
            sources,
            current_index: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            polling_interval,
            metrics: MultiRpcBlockSourceMetrics::default(),
        })
    }

    /// Get the next peer index using round-robin
    fn next_peer_index(&self) -> usize {
        self.current_index
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % self.sources.len()
    }

    /// Fetch a block with automatic failover to other peers
    async fn fetch_block_with_failover(&self, height: u64) -> eyre::Result<BlockAndReceipts> {
        let start_index = self.next_peer_index();
        let mut last_error = None;

        for attempt in 0..self.sources.len() {
            let peer_index = (start_index + attempt) % self.sources.len();
            let source = &self.sources[peer_index];

            match source.fetch_block(height).await {
                Ok(block) => {
                    self.metrics.fetched.increment(1);
                    if attempt > 0 {
                        debug!(height, peer_index, attempts = attempt + 1, "Fetched block after failover");
                    }
                    return Ok(block);
                }
                Err(e) => {
                    self.metrics.errors.increment(1);
                    if attempt < self.sources.len() - 1 {
                        self.metrics.failovers.increment(1);
                        debug!(height, peer_index, error = %e, "Peer failed, trying next");
                    }
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| eyre::eyre!("All peers failed")))
    }
}

impl BlockSource for MultiRpcBlockSource {
    fn collect_block(&self, height: u64) -> BoxFuture<'static, eyre::Result<BlockAndReceipts>> {
        let this = self.clone();
        async move { this.fetch_block_with_failover(height).await }.boxed()
    }

    fn find_latest_block_number(&self) -> BoxFuture<'static, Option<u64>> {
        // Query the first available peer
        let sources = self.sources.clone();
        async move {
            for source in &sources {
                if let Some(num) = source.find_latest_block_number().await {
                    return Some(num);
                }
            }
            None
        }
        .boxed()
    }

    fn recommended_chunk_size(&self) -> u64 {
        1000
    }

    fn polling_interval(&self) -> Duration {
        self.polling_interval
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Block 1 from mainnet (empty block) - from debug_getRawBlock RPC
    const BLOCK_1_RAW: &str = "0xf9034df90347f9023da0d8fcc13b6a195b88b7b2da3722ff6cad767b13a8c1e9ffb1c73aa9d216d895f0a01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347940000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000000000000a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421b90100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000008001831e8480808467b4003480a000000000000000000000000000000000000000000000000000000000000000008800000000000000008405f5e100a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b4218080a00000000000000000000000000000000000000000000000000000000000000000f90104b901000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000080c0c0c0";

    // Block 1715 from mainnet (contains a system transaction)
    const BLOCK_1715_RAW: &str = "0xf903c9f9034bf90241a07cba18ba47b6944af7e4229f938396a4b94fb904ce751173902d3641b6b2f92ea01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347940000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000000000000a054ffcff2a59b8e44e86ef5e0970e3282221c824dce31c511c1c49bada2b1d7dea0f78dfb743fbd92ade140711c8bbc542b5e307f0ab7984eff35d751969fe57efab9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000808206b3831e84808252088467b40d2880a000000000000000000000000000000000000000000000000000000000000000008800000000000000008405f5e100a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b4218080a00000000000000000000000000000000000000000000000000000000000000000f90104b901000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000080f874b87202f86f8203e78001840bebc2018252089458c3b96d5eed8b01172a394d8916245f4fe04edf872386f26fc1000080c001a0b330e7f3ce637a56510d3f80fb44387dcf058b7168aad8a6f4b1e883490ddd5ea05e8da2a9aec3ec2eae56f71f455d4fa7010ba737d4ac2be396b6a009a7efe58dc0c0808190";

    #[test]
    fn test_hex_decode() {
        let hex = "0x48656c6c6f";
        let bytes = hex_decode(hex).unwrap();
        assert_eq!(bytes, b"Hello");

        let hex_no_prefix = "48656c6c6f";
        let bytes = hex_decode(hex_no_prefix).unwrap();
        assert_eq!(bytes, b"Hello");
    }

    #[test]
    fn test_decode_block_1_empty() {
        let block = decode_raw_block(BLOCK_1_RAW).unwrap();

        assert_eq!(block.header.number, 1);
        assert_eq!(block.body.inner.transactions.len(), 0);
        assert!(block.body.inner.ommers.is_empty());
    }

    #[test]
    fn test_decode_block_1715_with_transaction() {
        use alloy_consensus::Transaction as TxTrait;

        let block = decode_raw_block(BLOCK_1715_RAW).unwrap();

        assert_eq!(block.header.number, 1715);
        assert_eq!(block.body.inner.transactions.len(), 1, "Block 1715 should have 1 transaction");

        // Check system_tx_count from the header extras
        println!("Block 1715 system_tx_count: {}", block.header.extras.system_tx_count);

        // Check gas_price of the transaction
        let tx = &block.body.inner.transactions[0];
        println!("Transaction gas_price: {:?}", tx.gas_price());
        println!("Transaction is_system_tx (gas_price == 0): {}", tx.gas_price().map_or(false, |p| p == 0));

        // Verify the transaction is an EIP-1559 transaction
        assert!(matches!(tx.inner(), reth_primitives::TransactionSigned::Eip1559(_)));
    }

    #[test]
    fn test_convert_to_block_and_receipts_empty_block() {
        let block = decode_raw_block(BLOCK_1_RAW).unwrap();
        let receipts = vec![];
        let extras = HlExtras::default();

        let result = convert_to_block_and_receipts(block, receipts, extras).unwrap();

        match &result.block {
            crate::node::types::EvmBlock::Reth115(sealed) => {
                assert_eq!(sealed.header.header.number, 1);
                assert_eq!(sealed.body.transactions.len(), 0);
            }
        }
    }

    #[tokio::test]
    async fn test_rpc_block_source_fetch_block_1() {
        // Skip test if RPC endpoint is not available
        let rpc_url = std::env::var("TEST_RPC_URL")
            .unwrap_or_else(|_| "http://85.10.200.167:8545".to_string());

        let source = match RpcBlockSource::new(&rpc_url, Duration::from_millis(100)) {
            Ok(s) => s,
            Err(_) => {
                eprintln!("Skipping test: RPC endpoint not available");
                return;
            }
        };

        // Try to fetch block 1
        match source.fetch_block(1).await {
            Ok(block_and_receipts) => {
                match &block_and_receipts.block {
                    crate::node::types::EvmBlock::Reth115(sealed) => {
                        assert_eq!(sealed.header.header.number, 1);
                    }
                }
                println!("Successfully fetched block 1");
            }
            Err(e) => {
                panic!("Failed to fetch block 1: {:?}", e);
            }
        }
    }

    #[tokio::test]
    async fn test_rpc_block_source_fetch_block_1715() {
        // Skip test if RPC endpoint is not available
        let rpc_url = std::env::var("TEST_RPC_URL")
            .unwrap_or_else(|_| "http://85.10.200.167:8545".to_string());

        let source = match RpcBlockSource::new(&rpc_url, Duration::from_millis(100)) {
            Ok(s) => s,
            Err(_) => {
                eprintln!("Skipping test: RPC endpoint not available");
                return;
            }
        };

        // Fetch block 1715 which has a system transaction
        match source.fetch_block(1715).await {
            Ok(block_and_receipts) => {
                // Block 1715 should have 1 system tx and 0 regular txs
                assert_eq!(block_and_receipts.system_txs.len(), 1, "Block 1715 should have 1 system tx");

                match &block_and_receipts.block {
                    crate::node::types::EvmBlock::Reth115(sealed) => {
                        assert_eq!(sealed.header.header.number, 1715);
                        // After splitting, regular transactions should be 0
                        assert_eq!(sealed.body.transactions.len(), 0, "Block 1715 should have 0 regular txs after splitting");
                        println!("Block 1715 has {} system txs and {} regular txs",
                                 block_and_receipts.system_txs.len(),
                                 sealed.body.transactions.len());
                    }
                }
            }
            Err(e) => {
                panic!("Failed to fetch block 1715: {:?}", e);
            }
        }
    }
}
