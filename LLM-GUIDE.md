# axe — LLM Debugging Guide

This guide is for AI assistants (Claude, GPT, etc.) helping debug Axelar cross-chain infrastructure. It explains how to use `axe` commands to investigate stuck messages, decode transactions, and inspect on-chain state.

## Quick Reference

| Task | Command |
|---|---|
| Decode an EVM tx | `cargo r -- decode tx 0xabc...` |
| Decode a Solana tx | `cargo r -- decode tx <base58-signature>` |
| Decode raw calldata | `cargo r -- decode calldata 0x...` |
| Recent Solana gateway activity | `cargo r -- decode sol-activity --program gateway --network devnet-amplifier --limit 10` |
| Recent EVM gateway events | `cargo r -- decode evm-activity --contract gateway --network devnet-amplifier --chain avalanche-fuji --limit 10` |
| Solana gateway/ITS on-chain state | `cargo r -- decode sol-state` |
| Run a GMP test (Solana, no relayer) | `cargo r --no-default-features --features testnet -- test gmp --config ../axelar-contract-deployments/axelar-chains-config/info/testnet.json --source-chain solana --destination-chain solana` |

## Debugging a Stuck Message

### Step 1: Identify where it stopped

There are **two distinct flows** depending on whether the message is GMP (direct) or ITS (hub-routed):

#### Flow 1: GMP (General Message Passing) — direct chain-to-chain

Messages go directly from source to destination without the ITS hub:

```
Source chain (callContract)
  → Cosmos: verify_messages on source Gateway
  → Cosmos: verifiers vote on VotingVerifier poll
  → Cosmos: end_poll on VotingVerifier
  → Cosmos: route_messages on source Gateway → Router → dest Gateway
  → Cosmos: construct_proof on dest MultisigProver
  → Cosmos: verifiers sign proof
  → Dest chain: submit execute_data (approve + execute)
```

The destination chain is specified in the original `ContractCall` event. One leg only.

#### Flow 2: ITS (Interchain Token Service) — two-leg hub routing

ITS messages always route through the Axelar ITS hub on Cosmos. This creates **two separate legs** with different message IDs:

```
FIRST LEG (source → hub):
  Source chain (ContractCall with destination="axelar")
  → Cosmos: verify_messages on source Gateway
  → Cosmos: verifiers vote
  → Cosmos: route_messages → Router → AxelarnetGateway (marks as "approved")
  → Cosmos: execute on AxelarnetGateway → ITS Hub processes message

SECOND LEG (hub → destination):
  ITS Hub emits new message with source_chain="axelar" and NEW message_id
  → Cosmos: Router routes to dest Gateway (auto, no verification needed)
  → Cosmos: construct_proof on dest MultisigProver
  → Cosmos: verifiers sign proof
  → Dest chain: submit execute_data (approve + execute)
```

**Key differences from GMP:**
- The `ContractCall` destination is always `"axelar"` (the ITS hub), not the final destination
- The ITS hub creates a **second-leg message** with a new `message_id` and `source_chain="axelar"`
- To trace the second leg, use `tx_search` on the Axelar RPC for `wasm-message_executed.message_id='<first_leg_id>'` to find the `wasm-routing` event with the second-leg message_id
- The `ampd-event-verifier` pod (or relayer) is responsible for calling `execute` on the AxelarnetGateway — if it's down, all ITS messages get stuck at "Approving"

### Step 2: Decode the source transaction

```bash
# EVM source
cargo r -- decode tx 0x<tx_hash>

# Solana source
cargo r -- decode tx <solana-signature>
```

This shows: calldata/instruction decoded, event logs, payload contents. For ITS, it decodes the nested hub payload (SEND_TO_HUB → InterchainTransfer/DeployInterchainToken).

### Step 3: Check destination chain activity

```bash
# Solana gateway — see if init/verify/approve happened
cargo r -- decode sol-activity --program gateway --network devnet-amplifier --limit 20

# EVM gateway — see if ContractCall/MessageApproved/MessageExecuted happened
cargo r -- decode evm-activity --contract gateway --network devnet-amplifier --chain avalanche-fuji --limit 20
```

### Step 4: Check Cosmos pipeline (requires axelard or LCD)

```bash
# Check if message was verified on the VotingVerifier
# Query: {"messages_status": [{"cc_id": {"source_chain": "<chain>", "message_id": "<id>"}, ...}]}

# Check if message was routed
# Call route_messages on the source chain's Gateway

# Check if hub executed (for ITS)
# Query AxelarnetGateway executable_messages or use tx_search for wasm-message_executed
```

### Step 5: Check Solana gateway state

```bash
cargo r -- decode sol-state
```

Shows: epoch, verifier set hashes, ITS hub address, trusted chains, paused status.

## Network → Feature Flag Mapping

axe compiles different Solana program IDs based on the cargo feature:

| Network | Feature flag | Build command |
|---|---|---|
| devnet-amplifier | `devnet-amplifier` (default) | `cargo r -- ...` |
| stagenet | `stagenet` | `cargo r --no-default-features --features stagenet -- ...` |
| testnet | `testnet` | `cargo r --no-default-features --features testnet -- ...` |
| mainnet | `mainnet` | `cargo r --no-default-features --features mainnet -- ...` |

**If you build with the wrong feature, Solana program IDs will be wrong** and message ID extraction, PDA derivation, etc. will fail silently.

## Config Auto-Discovery

Most commands auto-discover chain configs from `../axelar-contract-deployments/axelar-chains-config/info/`. The directory structure:

```
workspace/
├── axe/                                    # this repo
└── axelar-contract-deployments/
    └── axelar-chains-config/info/
        ├── devnet-amplifier.json
        ├── stagenet.json
        ├── testnet.json
        └── mainnet.json
```

Each config has `chains.<name>.rpc`, `chains.<name>.contracts.AxelarGateway.address`, etc.

## JSON Mode for Programmatic Use

All activity commands support `--json` for structured output:

```bash
cargo r -- decode sol-activity --program gateway --network devnet-amplifier --limit 5 --json
cargo r -- decode evm-activity --contract gateway --network devnet-amplifier --chain avalanche-fuji --json
```

JSON output includes decoded instruction/event names, args as key-value pairs, tx hashes, block numbers — everything needed to trace a message through the pipeline.

## Common Issues

| Symptom | Likely cause | How to check |
|---|---|---|
| Verifiers vote NotFound | Wrong gateway address in VotingVerifier, or tx too old for RPC | Check `source_gateway_address` in poll event vs actual gateway |
| SignatureVerificationFailed on Solana | Recovery ID not normalized (27/28 → 0/1) | Decode the VerifySignature tx, check recovery_id byte |
| Message stuck at "Approving" | Hub execute not happening (relayer/ampd issue) | Check `ampd-event-verifier` logs, try manual `route_messages` + `execute` on AxelarnetGateway |
| Solana tx fails "Program failed to complete" | CU limit too low, or account layout mismatch after program upgrade | Add ComputeBudget instruction, check if program was upgraded |
| Poll ends with 0 votes | ampd event subscriber broken | Check `kubectl logs` for ampd pods, look for "stream timed out" or parse errors |

## Axelar Environment Details

| Environment | Cosmos chain_id | Fee denom | Solana RPC |
|---|---|---|---|
| devnet-amplifier | `devnet-amplifier` | `uamplifier` | `https://api.devnet.solana.com` |
| stagenet | (from config) | (from config) | `https://api.testnet.solana.com` |
| testnet | `axelar-testnet-lisbon-3` | `uaxl` | `https://api.devnet.solana.com` |
| mainnet | (from config) | `uaxl` | `https://api.mainnet-beta.solana.com` |

## Useful Cosmos Queries (via axelard or LCD)

```bash
# Check message verification status
curl LCD/cosmwasm/wasm/v1/contract/<VotingVerifier>/smart/<base64_query>
# Query: {"messages_status": [{...}]}

# End an expired poll
axelard tx wasm execute <VotingVerifier> '{"end_poll":{"poll_id":"<id>"}}'

# Route a verified message
axelard tx wasm execute <Gateway> '{"route_messages": [{...}]}'

# Construct proof for Solana
axelard tx wasm execute <MultisigProver> '{"construct_proof": [{"source_chain":"...","message_id":"..."}]}'

# Check proof status
# Query MultisigProver: {"proof":{"multisig_session_id":"<id>"}}
```
