#!/bin/bash

if [ -z "$1" ]; then
    echo "Usage: $0 <proposal_id> [--dry-run]"
    echo "Example: $0 123"
    echo "Example: $0 123 --dry-run"
    exit 1
fi
proposal_id="$1"
dry_run=false
if [ "$2" = "--dry-run" ]; then
    dry_run=true
fi

namespace="devnet-amplifier"

pods=$(kubectl get pods -n "$namespace" --no-headers | grep "validator" | grep -v "rogue" | awk '{print $1}')

if [ -z "$pods" ]; then
    echo "Error: No validator pods found in namespace '$namespace'"
    exit 1
fi

echo "Found pods:"
echo "$pods"
echo ""

for pod in $pods; do
    # alpha pod uses "axelar-core-node" container, others use "core"
    if echo "$pod" | grep -q "alpha"; then
        container="axelar-core-node"
    else
        container="core"
    fi

    cmd="kubectl exec -n $namespace -it $pod -c $container -- /bin/sh -c \"echo \\\"\\\$KEYRING_PASSWORD\\\" | axelard tx gov vote $proposal_id yes --from validator --gas 80000 --gas-adjustment 1.4\""
    if [ "$dry_run" = true ]; then
        echo "[DRY RUN] $cmd"
    else
        echo "Submitting vote for proposal $proposal_id on pod $pod (namespace: $namespace)"
        eval "$cmd"
    fi
    echo ""
done
