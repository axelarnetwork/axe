#!/usr/bin/env bash
#
# Run the polish-refactor baseline matrix: 12 chain × protocol combos at
# --num-txs 1, in serial, on testnet. Used to detect regressions between
# refactor phases.
#
# Usage:
#   scripts/run-baseline-matrix.sh <out-dir>
#
# Builds with --no-default-features --features testnet (load-test fails fast
# at startup if the config network doesn't match the compiled feature).
# Each combo emits axe-load-test-logs/axe-load-test-<epoch>.json; the script
# copies the newly written file to <out-dir>/<NN-src-dst-protocol>.json.

set -uo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <out-dir>" >&2
    exit 2
fi

OUT="$1"
mkdir -p "$OUT"

CONFIG="${CHAINS_CONFIG:-../axelar-contract-deployments/axelar-chains-config/info/testnet.json}"
BIN="./target/release/axe"

if [[ ! -x "$BIN" ]]; then
    echo "binary not found at $BIN — run: cargo build --release --no-default-features --features testnet" >&2
    exit 2
fi
if [[ ! -f "$CONFIG" ]]; then
    echo "config not found at $CONFIG (override with CHAINS_CONFIG)" >&2
    exit 2
fi

export SOLANA_PRIVATE_KEY="${SOLANA_PRIVATE_KEY:-${HOME}/.config/solana/id.json}"

# format: name protocol source destination
combos=(
    "01-sol-sol-gmp gmp solana solana"
    "02-sol-flow-gmp gmp solana flow"
    "03-flow-sol-gmp gmp flow solana"
    "04-flow-hedera-gmp gmp flow hedera"
    "05-sol-flow-its its solana flow"
    "06-flow-sol-its its flow solana"
    "07-flow-sui-gmp gmp flow sui"
    "08-sol-sui-gmp gmp solana sui"
    "09-flow-xrpl-its its flow xrpl"
    "10-xrpl-flow-its its xrpl flow"
    "11-flow-stellar-its its flow stellar-2026-q1-2"
    "12-stellar-flow-its its stellar-2026-q1-2 flow"
)

overall_start=$(date +%s)
echo "==> baseline matrix → $OUT"
echo "==> config: $CONFIG"
echo "==> combos: ${#combos[@]}"
echo

for entry in "${combos[@]}"; do
    read -r name proto src dst <<< "$entry"
    combo_start=$(date +%s)
    echo "[$name] $proto: $src -> $dst"

    log_file="$OUT/${name}.log"
    "$BIN" test load-test \
        --config "$CONFIG" \
        --num-txs 1 \
        --protocol "$proto" \
        --source-chain "$src" \
        --destination-chain "$dst" \
        > "$log_file" 2>&1
    rc=$?

    # The runner prints "report written to <path>" once it serializes the JSON.
    json_path=$(grep -oE 'report written to [^ ]+\.json' "$log_file" | tail -1 | awk '{print $4}')
    if [[ -n "$json_path" && -f "$json_path" ]]; then
        cp "$json_path" "$OUT/${name}.json"
        echo "    saved $OUT/${name}.json (rc=$rc, $(($(date +%s) - combo_start))s)"
    else
        echo "    NO REPORT JSON FOUND (rc=$rc, $(($(date +%s) - combo_start))s) — see $log_file"
    fi
    echo
done

echo "==> baseline matrix complete in $(($(date +%s) - overall_start))s"
