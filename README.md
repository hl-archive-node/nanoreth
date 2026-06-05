# nanoreth

HyperEVM archive node implementation based on [reth](https://github.com/paradigmxyz/reth).
NodeBuilder API version is heavily inspired by [reth-bsc](https://github.com/loocapro/reth-bsc).

Got questions? Drop by the [Hyperliquid Discord](https://discord.gg/hyperliquid) #node-operators channel.

## ⚠️ IMPORTANT: System Transactions Appear as Pseudo Transactions

Deposit transactions from [System Addresses](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/hyperevm/hypercore-less-than-greater-than-hyperevm-transfers#system-addresses) like `0x222..22` / `0x200..xx` to user addresses are intentionally recorded as pseudo transactions.
This change simplifies block explorers, making it easier to track deposit timestamps.
Ensure careful handling when indexing.

To disable this behavior, add `--hl-node-compliant` to the CLI arguments-this will not show system transactions and their receipts, mimicking hl-node's output.

### System transaction sender encoding

System transactions arrive from hl-node unsigned; nanoreth fabricates a signature that encodes the real `msg.sender` into its `s` value. Sender recovery decodes it back:

| `s` | recovered `msg.sender` |
| --- | --- |
| `1` | `0x2222...2222` (the HYPE system address) |
| `0x10000000000000000000000000000000000000001` | `0x00...01` |
| anything else | the low 20 bytes of `s` (e.g. spot-token `0x2000...00` senders) |

The middle row is an escape: a real sender of `0x00...01` would otherwise encode to `s == 1` and collide with the HYPE sentinel, so it is lifted to `(1 << 160) | 1` (which still decodes back via the low 20 bytes).

The RPC returns this recovered address as each transaction's `from`; together with the zero `gasPrice` and the table above, that lets you recognize system transactions. To confirm them authoritatively, use hl-node's [`eth_getSystemTxsByBlockHash` / `eth_getSystemTxsByBlockNumber`](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/hyperevm/json-rpc) methods, which return the system transactions originating from HyperCore, and source blocks only from a node you trust.

### Per-request compliance multiplexing

If you need a single endpoint to serve both filtered and unfiltered responses, use `--hl-node-compliant-multiplexed`. This adds a middleware layer that reads the `?hl=` query parameter from each request:

- `?hl=true` or `?hl=1` — apply HL compliance filtering (hide system transactions)
- `?hl=false` — disable filtering (show all transactions)
- No parameter — falls back to the `--hl-node-compliant` flag (default: unfiltered)

```sh
reth-hl node --hl-node-compliant-multiplexed --hl-node-compliant \
  --http --http.addr 0.0.0.0 --http.api eth,ots,net,web3 --s3

# Then query with per-request control:
# curl http://localhost:8545?hl=true   # filtered
# curl http://localhost:8545?hl=false  # unfiltered
# curl http://localhost:8545           # uses --hl-node-compliant default
```

## Prerequisites

Building nanoreth from source requires Rust and Cargo to be installed:

`$ curl https://sh.rustup.rs -sSf | sh`

## How to run (mainnet)

1) Setup AWS credentials at `~/.aws/credentials` with read S3 access.

2) `$ make install` - this will install the reth-hl binary.

3) Start nanoreth which will begin syncing using the blocks in `~/evm-blocks`:

```sh
reth-hl node --http --http.addr 0.0.0.0 --http.api eth,ots,net,web3 \
  --ws --ws.addr 0.0.0.0 --ws.origins '*' --ws.api eth,ots,net,web3 \
  --ws.port 8545 --http.port 8545 --s3
```

## How to run (mainnet) (with local block sync)

The `--s3` method above fetches blocks from S3, but you can instead source them from your local hl-node using `--ingest-dir` and `--local-ingest-dir`.

This will require you to first have a hl-node outputting blocks prior to running the initial s3 sync,
the node will prioritise locally produced blocks with a fallback to s3.
This method will allow you to reduce the need to rely on S3.

This setup adds `--local-ingest-dir=<path>` (or a shortcut: `--local` if using default hl-node path) to ingest blocks from hl-node, and `--ingest-dir` for fallback copy of EVM blocks. `--ingest-dir` can be replaced with `--s3` if you don't want to
periodically run `aws s3 sync` as below.

```sh
# Run your local hl-node (make sure output file buffering is disabled)
# Make sure evm blocks are being produced inside evm_block_and_receipts
$ hl-node run-non-validator --replica-cmds-style recent-actions --serve-eth-rpc --disable-output-file-buffering

# Fetch EVM blocks (Initial sync)
$ aws s3 sync s3://hl-mainnet-evm-blocks/ ~/evm-blocks --request-payer requester # one-time

# Run node (with local-ingest-dir arg)
$ make install
$ reth-hl node --http --http.addr 0.0.0.0 --http.api eth,ots,net,web3 \
    --ws --ws.addr 0.0.0.0 --ws.origins '*' --ws.api eth,ots,net,web3 --ingest-dir ~/evm-blocks --local-ingest-dir <path-to-your-hl-node-evm-blocks-dir> --ws.port 8545
```

## How to run (syncing from another nanoreth node via RPC)

If you already have a nanoreth node running (e.g. in the cloud with S3 access), you can sync a local node from it without needing S3 credentials.

**On the serving node** (cloud), add `--enable-sync-server` to its launch flags:

```sh
reth-hl node --http --http.addr 0.0.0.0 --http.api eth,ots,net,web3 \
  --ws --ws.addr 0.0.0.0 --ws.origins '*' --ws.api eth,ots,net,web3 \
  --ws.port 8545 --http.port 8545 --s3 --enable-sync-server
```

**On the local node**, point `--block-source` to the serving node's RPC:

```sh
reth-hl node --http --http.api eth,ots,net,web3 \
  --block-source=rpc://your-cloud-node:8545
```

The `--rpc.polling-interval` flag controls how often the local node polls for new blocks (default: 100ms).

## Architecture: How nanoreth differs from reth

Nanoreth replaces reth's native P2P sync pipeline with a **pseudo peer + block source** architecture:

- **Standard reth** syncs by downloading headers and bodies from P2P peers, then re-executing every transaction locally to produce receipts and state. The eth wire protocol only transfers headers and bodies — receipts are never sent over P2P because each node generates its own.
- **Nanoreth** does not re-execute blocks. Blocks arrive pre-executed from hl-node (the Go Hyperliquid node), so receipts must be **transferred** alongside blocks. Since the eth wire protocol has no mechanism for this, nanoreth fetches complete blocks (with receipts) from an external **block source** (S3, local files, or another nanoreth node via RPC). A "pseudo peer" process connects to the main node as a localhost P2P peer and serves these blocks when requested. Blocks are imported via the engine API (`new_payload` + `fork_choice_updated`), bypassing reth's download pipeline entirely.

This means reth's `--bootnodes` and `--trusted-peers` flags will establish P2P connections but **will not trigger historical block sync** — the sync pipeline stages that request blocks from peers are not active in nanoreth. A block source (`--s3`, `--local`, `--block-source`) is required for syncing.

Nanoreth also extends reth's block types with Hyperliquid-specific fields (`system_tx_count`, `read_precompile_calls`, `highest_precompile_address`, blob `sidecars`) that are not part of the standard Ethereum wire protocol, further requiring the custom sync path.

## How to run (testnet)

Testnet is supported since block 34112653.

```sh
# Get testnet genesis at block 34112653
$ cd ~
$ git clone https://github.com/sprites0/hl-testnet-genesis
$ zstd --rm -d ~/hl-testnet-genesis/*.zst

# Init node
$ make install
$ reth-hl init-state --without-evm --chain testnet --header ~/hl-testnet-genesis/34112653.rlp \
  --header-hash 0xeb79aca618ab9fda6d463fddd3ad439045deada1f539cbab1c62d7e6a0f5859a \
  ~/hl-testnet-genesis/34112653.jsonl --total-difficulty 0 

# Run node
$ reth-hl node --chain testnet --http --http.addr 0.0.0.0 --http.api eth,ots,net,web3 \
    --ws --ws.addr 0.0.0.0 --ws.origins '*' --ws.api eth,ots,net,web3 --ingest-dir ~/evm-blocks --ws.port 8546
```
