#!/bin/bash

identifier="$1"
if [ -z "$1" ]; then
    echo "Usage: $0 <identifier> <proposal_id>"
    echo "Example: $0 isaac 123"
    exit 1
fi
echo "Identifier: $identifier"

# proposal id
if [ -z "$2" ]; then
    echo "Usage: $0 <identifier> <proposal_id>"
    echo "Example: $0 isaac 123"
    exit 1
fi
proposal_id="$2"

# For devnets, the namespace pattern is "devnet-{identifier}"
namespace="${identifier}"

# Check if the namespace exists
if ! kubectl get namespace "$namespace" >/dev/null 2>&1; then
    echo "Error: Namespace '$namespace' not found"
    echo "Available devnet namespaces:"
    kubectl get namespaces | grep "^devnet-" | awk '{print $1}'
    exit 1
fi

# Get all validator pods in the namespace
pods=$(kubectl get pods -n "$namespace" | grep "validator-" | awk '{print $1}')

if [ -z "$pods" ]; then
    echo "Error: No validator pods found in namespace '$namespace'"
    echo "Available pods in namespace '$namespace':"
    kubectl get pods -n "$namespace"
    exit 1
fi

echo "Found validator pods in namespace '$namespace':"
echo "$pods"
echo ""

for pod in $pods; do
    echo "Submitting vote for proposal $proposal_id on pod $pod"
    kubectl exec -n "$namespace" -it "$pod" -c axelar-core-node -- /bin/sh -c "echo \"\$KEYRING_PASSWORD\" | axelard tx gov vote $proposal_id yes --from validator --gas 80000 --gas-adjustment 1.4"
    echo ""
done

echo "Vote submission completed for all validator pods in namespace '$namespace'"