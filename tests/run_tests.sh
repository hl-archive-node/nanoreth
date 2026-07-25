#!/bin/bash

set -e

success() {
    echo "Success: $1"
}

fail() {
    echo "Failed: $1"
    exit 1
}

ensure_cmd() {
    command -v "$1" > /dev/null 2>&1 || fail "$1 is required"
}

ensure_cmd jq
ensure_cmd cast
ensure_cmd wscat
ensure_cmd curl

if [[ -z "${ETH_RPC_URL:-}" ]]; then
    fail "ETH_RPC_URL must be set"
fi

if [[ -z "${TRACE_RPC_URL:-}" ]]; then
    fail "TRACE_RPC_URL must be set"
fi

if [[ ! "$ETH_RPC_URL" =~ ^https?:// && ! "$ETH_RPC_URL" =~ ^wss?:// ]]; then
    fail "ETH_RPC_URL must be an http(s) or ws(s) url"
fi

TRACE_RPC_URL="${TRACE_RPC_URL/wss:\/\//https:\/\/}"
TRACE_RPC_URL="${TRACE_RPC_URL/ws:\/\//http:\/\/}"

if [[ ! "$TRACE_RPC_URL" =~ ^https?:// ]]; then
    fail "TRACE_RPC_URL must be an http(s) url"
fi

TITLE="Issue #78 - eth_getLogs should return system transactions"
cast logs \
    --rpc-url "$ETH_RPC_URL" \
    --from-block 15312567 \
    --to-block 15312570 \
    --address 0x9fdbda0a5e284c32744d2f17ee5c74b284993463 \
    0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef \
    | grep -q "0x00000000000000000000000020000000000000000000000000000000000000c5" \
    && success "$TITLE" || fail "$TITLE"

TITLE="Issue #78 - eth_getBlockByNumber should return the same logsBloom as official RPC"
OFFICIAL_RPC="https://rpc.hyperliquid.xyz/evm"
A=$(cast block 1394092 --rpc-url "$ETH_RPC_URL" -f logsBloom | md5sum)
B=$(cast block 1394092 --rpc-url "$OFFICIAL_RPC" -f logsBloom | md5sum)
echo node "$A"
echo rpc\  "$B"
[[ "$A" == "$B" ]] && success "$TITLE" || fail "$TITLE"

TITLE="eth_subscribe newHeads via wscat"
if [[ "${WS_RPC_URL:-}" =~ ^wss?:// || "$ETH_RPC_URL" =~ ^wss?:// ]]; then
    CMD='{"jsonrpc":"2.0","id":1,"method":"eth_subscribe","params":["newHeads"]}'
    wscat -w 2 -c "${WS_RPC_URL:-$ETH_RPC_URL}" -x "$CMD" | tail -1 | jq -r .params.result.nonce | grep 0x \
        && success "$TITLE" || fail "$TITLE"
else
    echo "Skipped: $TITLE - set WS_RPC_URL to test websocket subscriptions"
fi

TITLE="trace_block should include all txs for block 34922247"
BLOCK=0x214df07
MISSING_REVERTED=0x0a799b330359f9655b819627cdc4dd600bad255bdeaca51cc9d3c636edfe8b4f
MISSING_SUCCESS=0xfefa762d83fda72df7a5d84d365502b663d83ffd5e138bef1cf0d54ed5779ede
BLOCK_TX_COUNT=$(curl -sS -H 'content-type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getBlockByNumber\",\"params\":[\"$BLOCK\",false]}" \
    "$ETH_RPC_URL" | jq -r '.result.transactions | length')
[[ "$BLOCK_TX_COUNT" =~ ^[0-9]+$ ]] || fail "$TITLE - failed to fetch block tx count"
TRACE_RESULT=$(curl -sS -H 'content-type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"trace_block\",\"params\":[\"$BLOCK\"]}" \
    "$TRACE_RPC_URL")
echo "$TRACE_RESULT" | jq -e \
    --argjson expected "$BLOCK_TX_COUNT" \
    --arg reverted "$MISSING_REVERTED" \
    --arg success "$MISSING_SUCCESS" \
    '
    (.error | not) and
    ([.result[]?.transactionHash] | unique) as $hashes |
    ($hashes | length) == $expected and
    ($hashes | index($reverted) != null) and
    ($hashes | index($success) != null)
    ' > /dev/null \
    && success "$TITLE" || fail "$TITLE - trace_block missing expected tx hashes"

TITLE="Issue #143 - unknown/unavailable blocks must return null instead of panicking"
UNKNOWN_HASH=0x1111111111111111111111111111111111111111111111111111111111111111
FUTURE_BLOCK=0x7fffffffff
for REQ in \
    "eth_getBlockReceipts:[\"$UNKNOWN_HASH\"]" \
    "eth_getBlockReceipts:[\"$FUTURE_BLOCK\"]" \
    "eth_getBlockByHash:[\"$UNKNOWN_HASH\",false]" \
    "eth_getBlockByNumber:[\"$FUTURE_BLOCK\",false]" \
    "eth_getBlockTransactionCountByHash:[\"$UNKNOWN_HASH\"]" \
    "eth_getBlockTransactionCountByNumber:[\"$FUTURE_BLOCK\"]" \
    "eth_getTransactionReceipt:[\"$UNKNOWN_HASH\"]" \
    ; do
    METHOD=${REQ%%:*}
    PARAMS=${REQ#*:}
    RESULT=$(curl -sS -H 'content-type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$METHOD\",\"params\":$PARAMS}" \
        "$ETH_RPC_URL")
    echo "$RESULT" | jq -e '(.error | not) and (.result == null)' > /dev/null \
        || fail "$TITLE - $METHOD returned $RESULT"
done
success "$TITLE"

TITLE="Issue #143 - the pending tag must not panic (locally built pending block has no header)"
for METHOD in eth_getBlockByNumber eth_getBlockTransactionCountByNumber; do
    PARAMS='["pending",false]'
    [[ "$METHOD" == eth_getBlockTransactionCountByNumber ]] && PARAMS='["pending"]'
    RESULT=$(curl -sS -H 'content-type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$METHOD\",\"params\":$PARAMS}" \
        "$ETH_RPC_URL")
    echo "$RESULT" | jq -e '(.error | not) and has("result")' > /dev/null \
        || fail "$TITLE - $METHOD returned $RESULT"
done
success "$TITLE"

TITLE="Issue #143 - a system tx receipt must return null instead of panicking"
# System txs are hidden in compliant mode, so their index is below the block's system tx count
# and the adjusted index used to underflow. Walk back from the head until a block with system
# txs turns up; they are sparse on testnet, so allow the window to be widened.
SCAN_FROM=${SYSTEM_TX_SCAN_FROM:-$(curl -sS -H 'content-type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' "$ETH_RPC_URL" \
    | jq -r '.result' | { read -r hex; printf '%d' "$hex"; })}
SCAN_BLOCKS=${SYSTEM_TX_SCAN_BLOCKS:-2000}
SYSTEM_TX=""
for ((i = 0; i < SCAN_BLOCKS; i++)); do
    BLOCK=$(printf '0x%x' $((SCAN_FROM - i)))
    SYSTEM_TX=$(curl -sS -H 'content-type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getBlockReceiptsWithSystemTx\",\"params\":[\"$BLOCK\"]}" \
        "$ETH_RPC_URL" | jq -r '.result.systemTxReceipts[0].transactionHash // empty')
    [[ -n "$SYSTEM_TX" ]] && break
done
if [[ -n "$SYSTEM_TX" ]]; then
    RESULT=$(curl -sS -H 'content-type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getTransactionReceipt\",\"params\":[\"$SYSTEM_TX\"]}" \
        "$ETH_RPC_URL")
    echo "$RESULT" | jq -e '(.error | not) and (.result == null)' > /dev/null \
        && success "$TITLE ($SYSTEM_TX)" || fail "$TITLE - $SYSTEM_TX returned $RESULT"
else
    echo "Skipped: $TITLE - no system tx in the last $SCAN_BLOCKS blocks;" \
        "set SYSTEM_TX_SCAN_FROM/SYSTEM_TX_SCAN_BLOCKS to widen the search"
fi
