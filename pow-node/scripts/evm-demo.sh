#!/usr/bin/env bash
set -euo pipefail

# Run against a running development Breeja node. This key is public test data.
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
for tool in forge cast jq; do
  command -v "$tool" >/dev/null || { echo "Missing required tool: $tool" >&2; exit 1; }
done
RPC_URL="${RPC_URL:-http://127.0.0.1:8545}"
DEMO_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
DEMO_ACCOUNT=0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266
PROJECT=examples/evm-demo
forge build --root "$PROJECT"
ARTIFACT="$PROJECT/out/Counter.sol/Counter.json"
INITCODE="$(jq -r '.bytecode.object' "$ARTIFACT")"
RUNTIME="$(jq -r '.deployedBytecode.object' "$ARTIFACT")"
# Keep the offline Rust integration test's compiled fixtures reproducible.
test "$INITCODE" = "$(cat "$PROJECT/Counter.init.hex")"
test "$RUNTIME" = "$(cat "$PROJECT/Counter.runtime.hex")"

DEPLOYMENT="$(cast send --legacy --rpc-url "$RPC_URL" --private-key "$DEMO_KEY" \
  --timeout 60 --json --create "$INITCODE")"
jq -e '.status == "0x1" and .contractAddress != null' <<<"$DEPLOYMENT" >/dev/null
CONTRACT="$(jq -r '.contractAddress' <<<"$DEPLOYMENT")"
test "$(cast code --rpc-url "$RPC_URL" "$CONTRACT")" = "$RUNTIME"

# Gas price, nonce and gas limit are filled by Breeja's RPC methods.
RECEIPT="$(cast send --legacy --rpc-url "$RPC_URL" --private-key "$DEMO_KEY" \
  --timeout 60 --json "$CONTRACT" 'set(uint256)' 42)"
jq -e '.status == "0x1" and (.logs | length) == 1 and
  (.logsBloom | test("[1-9a-fA-F]"))' <<<"$RECEIPT" >/dev/null
test "$(cast call --rpc-url "$RPC_URL" "$CONTRACT" 'value()(uint256)')" = 42
EXPECTED="$(printf '0x%064x' 42)"
test "$(cast storage --rpc-url "$RPC_URL" "$CONTRACT" 0)" = "$EXPECTED"

# A simulated write must leave the stored value at 42.
cast call --rpc-url "$RPC_URL" --from "$DEMO_ACCOUNT" "$CONTRACT" 'set(uint256)' 7 >/dev/null
test "$(cast call --rpc-url "$RPC_URL" "$CONTRACT" 'value()(uint256)')" = 42
TOPIC="$(cast keccak 'ValueChanged(address,uint256)')"
BLOCK="$(jq -r '.blockNumber' <<<"$RECEIPT")"
FILTER="$(jq -nc --arg address "$CONTRACT" --arg block "$BLOCK" --arg topic "$TOPIC" \
  '{fromBlock:$block,toBlock:$block,address:$address,topics:[$topic]}')"
cast rpc --rpc-url "$RPC_URL" eth_getLogs "$FILTER" | jq -e 'length == 1' >/dev/null
printf 'Solidity demo passed: contract=%s value=42 block=%s\n' "$CONTRACT" "$BLOCK"
jq '{transactionHash,status,gasUsed,effectiveGasPrice}' <<<"$RECEIPT"
