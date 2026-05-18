#!/usr/bin/env bash
# scripts/test_amplifier_routes.sh
#
# Smoke-test every wired axe route for a given (network, protocol) in parallel,
# then print a tick/cross summary. The same script is invoked by the GitHub
# Actions workflow (.github/workflows/test-amplifier-routes.yml) and locally
# from the developer's terminal.
#
# Usage:
#   NETWORK=testnet PROTOCOL=its ./scripts/test_amplifier_routes.sh
#
# Required env vars:
#   NETWORK     mainnet | testnet | stagenet | devnet-amplifier
#   PROTOCOL    gmp | its
#
# Optional env vars:
#   NUM_TXS     transactions per route (default: 1)
#   AXE_BIN     path to axe binary or name on PATH (default: "axe")
#   CONFIG_DIR  directory holding <network>.json chains-config files
#               (default: ../axelar-contract-deployments/axelar-chains-config/info)
#   RESULTS_DIR where per-route logs land (default: mktemp -d)
#
# Exit status:
#   0  every route succeeded
#   1  at least one route failed (skipped routes don't count as failures)

set -euo pipefail

# ---------------------------------------------------------------------------
# Inputs + preflight
# ---------------------------------------------------------------------------

: "${NETWORK:?NETWORK env var required (mainnet|testnet|stagenet|devnet-amplifier)}"
: "${PROTOCOL:?PROTOCOL env var required (gmp|its)}"

NUM_TXS="${NUM_TXS:-1}"
AXE_BIN="${AXE_BIN:-axe}"
CONFIG_DIR="${CONFIG_DIR:-../axelar-contract-deployments/axelar-chains-config/info}"

case "$NETWORK" in
    mainnet|testnet|stagenet|devnet-amplifier) ;;
    *) echo "::error::Invalid NETWORK: $NETWORK" >&2; exit 1 ;;
esac
case "$PROTOCOL" in
    gmp|its|its-with-data) ;;
    *) echo "::error::Invalid PROTOCOL: $PROTOCOL" >&2; exit 1 ;;
esac

CONFIG_PATH="$CONFIG_DIR/$NETWORK.json"
if [ ! -f "$CONFIG_PATH" ]; then
    echo "::error::Chains config not found at $CONFIG_PATH" >&2
    exit 1
fi

if ! command -v "$AXE_BIN" >/dev/null 2>&1 && [ ! -x "$AXE_BIN" ]; then
    echo "::error::axe binary not found: $AXE_BIN (set AXE_BIN env or add axe to PATH)" >&2
    exit 1
fi

if ! command -v jq >/dev/null 2>&1; then
    echo "::error::jq is required for this script" >&2
    exit 1
fi

RESULTS_DIR="${RESULTS_DIR:-$(mktemp -d -t axe-amplifier-routes.XXXXXX)}"
mkdir -p "$RESULTS_DIR"

# ---------------------------------------------------------------------------
# Embedded mappings — keep in sync with .github/actions/run-loadtest/action.yml
# ---------------------------------------------------------------------------

# display name -> per-network axelar ID
CHAIN_MAP=$(cat <<'EOF'
{
  "Arbitrum":    {"mainnet": "arbitrum",   "testnet": "arbitrum-sepolia", "stagenet": "arbitrum-sepolia"},
  "Avalanche":   {"mainnet": "avalanche",  "testnet": "avalanche",        "stagenet": "avalanche",  "devnet-amplifier": "avalanche-fuji"},
  "Base":        {"mainnet": "base",       "testnet": "base-sepolia",     "stagenet": "base-sepolia"},
  "Ethereum":    {"mainnet": "ethereum",   "testnet": "ethereum-sepolia", "stagenet": "ethereum-sepolia"},
  "Flow":        {"mainnet": "flow",       "testnet": "flow",             "stagenet": "flow", "devnet-amplifier": "flow"},
  "Hedera":      {"testnet": "hedera"},
  "Hyperliquid": {"mainnet": "hyperliquid","testnet": "hyperliquid",      "stagenet": "hyperliquid"},
  "Monad":       {"mainnet": "monad",      "testnet": "monad-3",          "stagenet": "monad"},
  "Optimism":    {"mainnet": "optimism",   "testnet": "optimism-sepolia", "stagenet": "optimism-sepolia"},
  "Solana":      {"mainnet": "solana",     "testnet": "solana",           "stagenet": "solana-stagenet-3", "devnet-amplifier": "solana-18"},
  "Stellar":     {"mainnet": "stellar",    "testnet": "stellar-2026-q1-2"},
  "Sui":         {"mainnet": "sui",        "testnet": "sui",              "stagenet": "sui", "devnet-amplifier": "sui-2"},
  "XRPL":        {"mainnet": "xrpl",       "testnet": "xrpl",             "stagenet": "xrpl", "devnet-amplifier": "xrpl-dev"},
  "XRPL EVM":    {"mainnet": "xrpl-evm",   "testnet": "xrpl-evm",         "stagenet": "xrpl-evm"}
}
EOF
)

# display name -> axe-side chain type bucket (drives the route check)
CHAIN_TYPES=$(cat <<'EOF'
{
  "Arbitrum":    "evm",
  "Avalanche":   "evm",
  "Base":        "evm",
  "Ethereum":    "evm",
  "Flow":        "evm",
  "Hedera":      "evm",
  "Hyperliquid": "evm",
  "Monad":       "evm",
  "Optimism":    "evm",
  "Solana":      "sol",
  "Stellar":     "stellar",
  "Sui":         "sui",
  "XRPL":        "xrpl",
  "XRPL EVM":    "evm"
}
EOF
)

# Mirrors the dispatcher in src/commands/load_test/mod.rs:218.
ROUTES_MAP=$(cat <<'EOF'
{
  "gmp": {
    "evm":     ["evm", "sol", "sui", "stellar"],
    "sol":     ["evm", "sol", "sui", "stellar"],
    "stellar": ["evm", "sol", "sui"],
    "sui":     ["evm"]
  },
  "its": {
    "evm":     ["evm", "sol", "stellar", "sui", "xrpl"],
    "sol":     ["evm", "sui"],
    "stellar": ["evm", "sol", "sui"],
    "sui":     ["evm"],
    "xrpl":    ["evm"]
  },
  "its-with-data": {
    "evm": ["sol"]
  }
}
EOF
)

# ---------------------------------------------------------------------------
# Per-(NETWORK, PROTOCOL) route fleet
# ---------------------------------------------------------------------------

# Fleet design philosophy:
#   * Hyperliquid is the hub EVM — it pairs with Stellar, Solana, and Sui
#     so each non-EVM chain gets src+dst coverage without us having to
#     wire multiple EVM chains.
#   * Sui appears in pairs but for ITS it can only be a source (Sui as
#     ITS destination is unwired in axe's dispatcher — every `*→Sui` ITS
#     arm bails in src/commands/load_test/mod.rs).
#   * Hyperliquid is a source in multiple ITS routes (H→Stellar, H→Solana)
#     and multiple GMP routes (H→Stellar, H→Solana, H→Sui). The dispatch
#     loop below groups routes by source chain and serializes within each
#     group so same-wallet nonce races don't happen.
case "$NETWORK/$PROTOCOL" in
    testnet/its)
        # Two hubs and the XRPL pair:
        #   * XRPL ↔ XRPL EVM — the only XRPL-touching ITS routes (XRPL is
        #     source/dest only via XRPL EVM at the EVM-bridge layer).
        #   * Hyperliquid ↔ Stellar / Solana, plus Sui → Hyperliquid (Sui as
        #     ITS destination is unwired in axe's dispatcher).
        #   * Hedera ↔ Stellar / Solana, plus Sui → Hedera (Sui-as-ITS-dest
        #     is unwired). Hedera ↔ Hyperliquid and Hedera ↔ XRPL EVM are
        #     EVM-to-EVM ITS — dispatched via its_evm_to_evm.
        #     ITS Hedera→peer routes additionally require the peer's ITS to
        #     add "hedera" to its trusted-chains set; today only some peers
        #     do, so a subset of the Hedera→peer routes is expected to fail
        #     with UntrustedChain() at the destination until the trust is
        #     added upstream. axe surfaces that as a CANNOT_EXECUTE_MESSAGE
        #     failure.
        FLEET=$(cat <<'EOF'
[
  {"name":"XRPL -> XRPL EVM","src":"XRPL","dst":"XRPL EVM"},
  {"name":"XRPL EVM -> XRPL","src":"XRPL EVM","dst":"XRPL"},
  {"name":"Hyperliquid -> Stellar","src":"Hyperliquid","dst":"Stellar"},
  {"name":"Stellar -> Hyperliquid","src":"Stellar","dst":"Hyperliquid"},
  {"name":"Hyperliquid -> Solana","src":"Hyperliquid","dst":"Solana"},
  {"name":"Solana -> Hyperliquid","src":"Solana","dst":"Hyperliquid"},
  {"name":"Sui -> Hyperliquid","src":"Sui","dst":"Hyperliquid"},
  {"name":"Hedera -> Stellar","src":"Hedera","dst":"Stellar"},
  {"name":"Stellar -> Hedera","src":"Stellar","dst":"Hedera"},
  {"name":"Hedera -> Solana","src":"Hedera","dst":"Solana"},
  {"name":"Solana -> Hedera","src":"Solana","dst":"Hedera"},
  {"name":"Sui -> Hedera","src":"Sui","dst":"Hedera"},
  {"name":"Hedera -> Hyperliquid","src":"Hedera","dst":"Hyperliquid"},
  {"name":"Hyperliquid -> Hedera","src":"Hyperliquid","dst":"Hedera"},
  {"name":"Hedera -> XRPL EVM","src":"Hedera","dst":"XRPL EVM"},
  {"name":"XRPL EVM -> Hedera","src":"XRPL EVM","dst":"Hedera"}
]
EOF
)
        ;;
    testnet/gmp)
        # Two hubs:
        #   * Hyperliquid bidirectional with Stellar / Solana / Sui (XRPL has no
        #     GMP layer so it's absent in this fleet).
        #   * Hedera bidirectional with the same three non-EVM chains, plus
        #     Hedera↔Hyperliquid so the EVM-to-EVM dispatch path is exercised
        #     for Hedera too.
        # Hyperliquid and Hedera are each source in multiple routes — the
        # source-chain grouping below serializes within each group so the
        # same-wallet nonce races don't happen.
        FLEET=$(cat <<'EOF'
[
  {"name":"Hyperliquid -> Stellar","src":"Hyperliquid","dst":"Stellar"},
  {"name":"Stellar -> Hyperliquid","src":"Stellar","dst":"Hyperliquid"},
  {"name":"Hyperliquid -> Solana","src":"Hyperliquid","dst":"Solana"},
  {"name":"Solana -> Hyperliquid","src":"Solana","dst":"Hyperliquid"},
  {"name":"Hyperliquid -> Sui","src":"Hyperliquid","dst":"Sui"},
  {"name":"Sui -> Hyperliquid","src":"Sui","dst":"Hyperliquid"},
  {"name":"Hedera -> Hyperliquid","src":"Hedera","dst":"Hyperliquid"},
  {"name":"Hyperliquid -> Hedera","src":"Hyperliquid","dst":"Hedera"},
  {"name":"Hedera -> Stellar","src":"Hedera","dst":"Stellar"},
  {"name":"Stellar -> Hedera","src":"Stellar","dst":"Hedera"},
  {"name":"Hedera -> Solana","src":"Hedera","dst":"Solana"},
  {"name":"Solana -> Hedera","src":"Solana","dst":"Hedera"},
  {"name":"Hedera -> Sui","src":"Hedera","dst":"Sui"},
  {"name":"Sui -> Hedera","src":"Sui","dst":"Hedera"}
]
EOF
)
        ;;
    *)
        echo "::error::No route fleet defined for $NETWORK/$PROTOCOL yet — extend the case block in $0" >&2
        exit 1
        ;;
esac

# ---------------------------------------------------------------------------
# Per-route runner (background-able)
# ---------------------------------------------------------------------------

# Pipe-delimited status format: "<display name>|<status>|<detail>".
# Status is one of: success, failure, skipped.
run_route() {
    local idx="$1" name="$2" src_display="$3" dst_display="$4"
    local logfile="$RESULTS_DIR/$idx.log"
    local statusfile="$RESULTS_DIR/$idx.status"

    # Resolve display name -> network-specific axelar ID
    local src_id dst_id
    src_id=$(echo "$CHAIN_MAP" | jq -r --arg n "$src_display" --arg net "$NETWORK" '.[$n][$net] // empty')
    dst_id=$(echo "$CHAIN_MAP" | jq -r --arg n "$dst_display" --arg net "$NETWORK" '.[$n][$net] // empty')
    if [ -z "$src_id" ] || [ -z "$dst_id" ]; then
        printf '%s|skipped|chain not configured on %s\n' "$name" "$NETWORK" > "$statusfile"
        return
    fi

    # Cross-check against the dispatcher's supported route matrix
    local src_type dst_type allowed
    src_type=$(echo "$CHAIN_TYPES" | jq -r --arg c "$src_display" '.[$c] // empty')
    dst_type=$(echo "$CHAIN_TYPES" | jq -r --arg c "$dst_display" '.[$c] // empty')
    allowed=$(echo "$ROUTES_MAP" | jq -r --arg p "$PROTOCOL" --arg s "$src_type" \
                                       --arg d "$dst_type" '.[$p][$s] // [] | index($d)')
    if [ "$allowed" = "null" ]; then
        printf '%s|skipped|%s route not wired in dispatcher (%s -> %s)\n' \
            "$name" "$PROTOCOL" "$src_type" "$dst_type" > "$statusfile"
        return
    fi

    # Run axe — capture stdout+stderr to per-route log
    local start_ts elapsed exit_code
    start_ts=$(date +%s)
    set +e
    "$AXE_BIN" test load-test \
        --config "$CONFIG_PATH" \
        --source-chain "$src_id" \
        --destination-chain "$dst_id" \
        --protocol "$PROTOCOL" \
        --num-txs "$NUM_TXS" \
        > "$logfile" 2>&1
    exit_code=$?
    set -e
    elapsed=$(( $(date +%s) - start_ts ))

    if [ "$exit_code" -eq 0 ]; then
        printf '%s|success|%ds\n' "$name" "$elapsed" > "$statusfile"
    else
        printf '%s|failure|%ds (exit %d)\n' "$name" "$elapsed" "$exit_code" > "$statusfile"
    fi
}

# ---------------------------------------------------------------------------
# Fan out + wait
# ---------------------------------------------------------------------------

echo "Amplifier route fleet — $NETWORK / $PROTOCOL"
echo "axe binary:  $AXE_BIN"
echo "config:      $CONFIG_PATH"
echo "results dir: $RESULTS_DIR"
echo ""

# Routes are grouped by source chain. Routes within a group run sequentially
# (so they don't race on the source wallet's nonce — axe deploys a fresh
# AXE/SenderReceiver from the source chain's main wallet on first run, and
# two concurrent deploys from the same wallet collide). Groups run in
# parallel across distinct source chains.
declare -A SRC_GROUPS
declare -a NAMES SRCS DSTS
i=0
while IFS= read -r route_json; do
    NAMES[$i]=$(echo "$route_json" | jq -r '.name')
    SRCS[$i]=$( echo "$route_json" | jq -r '.src')
    DSTS[$i]=$( echo "$route_json" | jq -r '.dst')
    SRC_GROUPS[${SRCS[$i]}]+="$i "
    i=$((i + 1))
done < <(echo "$FLEET" | jq -c '.[]')
TOTAL=$i

echo "Dispatching $TOTAL routes in ${#SRC_GROUPS[@]} source-chain groups"
echo "(groups run in parallel; routes within a group run sequentially):"
# Loop with `while IFS= read -r` (not `for src in $(...)`) so source chain
# names containing spaces (e.g. "XRPL EVM") aren't word-split.
while IFS= read -r src; do
    indices="${SRC_GROUPS[$src]}"
    # shellcheck disable=SC2086
    count=$(echo $indices | wc -w)
    echo "  [$src] $count route(s):"
    for idx in $indices; do
        echo "    -> ${NAMES[$idx]}"
    done
done < <(printf '%s\n' "${!SRC_GROUPS[@]}" | sort)
echo ""

PIDS=()
while IFS= read -r src; do
    indices="${SRC_GROUPS[$src]}"
    (
        # shellcheck disable=SC2086
        for idx in $indices; do
            run_route "$idx" "${NAMES[$idx]}" "${SRCS[$idx]}" "${DSTS[$idx]}"
        done
    ) &
    PIDS+=($!)
done < <(printf '%s\n' "${!SRC_GROUPS[@]}")

echo "${#PIDS[@]} groups started (pids: ${PIDS[*]}). Waiting..."
echo ""

# Wait for all background group runners. `wait` returns the exit code of the
# last one; we don't care — per-route exit code lives in the status file.
wait || true

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

echo "==============================================="
echo "  Amplifier route fleet — $NETWORK / $PROTOCOL"
echo "==============================================="
passed=0; failed=0; skipped=0
for ((idx = 0; idx < TOTAL; idx++)); do
    statusfile="$RESULTS_DIR/$idx.status"
    if [ ! -f "$statusfile" ]; then
        echo "  ?  (no status file for route #$idx)"
        failed=$((failed + 1))
        continue
    fi
    IFS='|' read -r name status detail < "$statusfile"
    case "$status" in
        success)
            printf '  ✓  %-24s (%s)\n' "$name" "$detail"
            passed=$((passed + 1)) ;;
        failure)
            printf '  ✗  %-24s (%s)\n' "$name" "$detail"
            failed=$((failed + 1)) ;;
        skipped)
            printf '  -  %-24s (%s)\n' "$name" "$detail"
            skipped=$((skipped + 1)) ;;
        *)
            printf '  ?  %-24s (unknown status: %s)\n' "$name" "$status"
            failed=$((failed + 1)) ;;
    esac
done
echo "-----------------------------------------------"
printf '  %d passed, %d failed, %d skipped (out of %d)\n' \
    "$passed" "$failed" "$skipped" "$TOTAL"
echo ""
echo "Per-route logs: $RESULTS_DIR/<idx>.log"

[ "$failed" -eq 0 ] || exit 1
