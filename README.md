# nanoreth

Hyperliquid archive node based on [reth](https://github.com/paradigmxyz/reth).

## ⚠️ IMPORTANT: System Transactions Appear as Pseudo Transactions

Deposit transactions from `0x222..22` to user addresses are intentionally recorded as pseudo transactions.
This change simplifies block explorers, making it easier to track deposit timestamps.
Ensure careful handling when indexing.

## How to run (mainnet)

```sh
# Fetch EVM blocks
$ aws s3 sync s3://hl-mainnet-evm-blocks/ ~/evm-blocks --request-payer requester # one-time
$ goofys --region=ap-northeast-1 --requester-pays hl-mainnet-evm-blocks evm-blocks-bak # realtime

# Run node
$ make install
$ reth node --http --http.addr 0.0.0.0 --http.api eth,ots,net,web3 \
    --ws --ws.addr 0.0.0.0 --ws.origins '*' --ws.api eth,ots,net,web3 --ingest-dir ~/evm-blocks --ws.port 8545
```

## How to run (mainnet) (with local block sync) 

You can choose to source blocks from your local instance of hl-node instead of relying on an s3 replica.
This will require you to first have a hl-node outputting blocks prior to running the initial s3 sync,
the node will prioritise locally produced blocks with a fallback to s3.
This method will allow you to reduce the need to rely on goofys.

It is recommended that you periodically sync evm-blocks from s3 so you have a fallback in case your hl-node fails, as hl-node
will not backfill evm blocks.
```sh
# Run your local hl-node (make sure output file buffering is disabled)
# Make sure evm blocks are being produced inside evm_block_and_receipts
$ hl-node run-non-validator --replica-cmds-style recent-actions --serve-eth-rpc --disable-output-file-buffering

# Fetch EVM blocks (Initial sync)
$ aws s3 sync s3://hl-mainnet-evm-blocks/ ~/evm-blocks --request-payer requester # one-time

# Run node (with local-ingest-dir arg)
$ make install
$ reth node --http --http.addr 0.0.0.0 --http.api eth,ots,net,web3 \
    --ws --ws.addr 0.0.0.0 --ws.origins '*' --ws.api eth,ots,net,web3 --ingest-dir ~/evm-blocks --local-ingest-dir <path-to-your-hl-node-evm-blocks-dir> --ws.port 8545
```

## How to run (testnet)

Testnet is supported since block 21304281.

```sh
# Get testnet genesis at block 21304281
$ cd ~
$ git clone https://github.com/sprites0/hl-testnet-genesis
$ zstd --rm -d ~/hl-testnet-genesis/*.zst

# Init node
$ make install
$ reth init-state --without-evm --chain testnet --header ~/hl-testnet-genesis/21304281.rlp \
  --header-hash 0x5b10856d2b1ad241c9bd6136bcc60ef7e8553560ca53995a590db65f809269b4 \
  ~/hl-testnet-genesis/21304281.jsonl --total-difficulty 0 

# Run node
$ reth node --chain testnet --http --http.addr 0.0.0.0 --http.api eth,ots,net,web3 \
    --ws --ws.addr 0.0.0.0 --ws.origins '*' --ws.api eth,ots,net,web3 --ingest-dir ~/evm-blocks --ws.port 8546
```
