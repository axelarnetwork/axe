# axe

Swiss army knife CLI for Axelar development.

## Quick Start

```bash
# 1. Clone the contract deployments repo as a sibling
git clone https://github.com/axelarnetwork/axelar-contract-deployments.git
cd axelar-contract-deployments && npm install && cd ..

# 2. Configure
cp axe/.env.example axe/.env
# Edit .env with your chain details, keys, and mnemonics

# 3. Build and install
cd axe
cargo build && cp target/debug/axe ~/.cargo/bin/axe

# 4. Initialize and deploy
axe deploy init
axe deploy run
```

```
workspace/
├── axe/
└── axelar-contract-deployments/
```

## Commands

| Command              | Description                                   | Relayer |
| -------------------- | --------------------------------------------- | ------- |
| `axe deploy init`    | Initialize a new chain deployment from `.env` | -       |
| `axe deploy run`     | Run all deployment steps sequentially         | -       |
| `axe deploy status`  | Show deployment progress                      | -       |
| `axe deploy reset`   | Reset all steps to pending                    | -       |
| `axe test gmp`       | End-to-end GMP loopback test                  | no      |
| `axe test its`       | Deploy + transfer an interchain token         | no      |
| `axe test load-test` | Cross-chain load test                         | yes     |
| `axe decode calldata`| Decode raw EVM calldata                       | -       |
| `axe decode tx`      | Fetch & decode a full EVM transaction          | -       |

## Deploy

```bash
axe deploy run          # runs all 23 steps sequentially
axe deploy status       # shows progress
axe deploy reset        # start over
```

## Test GMP

### EVM (legacy mode)

```bash
axe test gmp
```

Sends a loopback GMP message on a deployed EVM chain and relays it through the full Amplifier pipeline end-to-end.

### Solana manual relaying

```bash
cargo run --no-default-features --features testnet -- test gmp --config ../axelar-contract-deployments/axelar-chains-config/info/testnet.json --source-chain solana --destination-chain solana
```

Sends a GMP message from Solana and manually relays it through the full Amplifier pipeline end-to-end: callContract → verify_messages → vote → end_poll → route_messages → construct_proof → approve on Solana gateway (init verification session → verify all 12 signatures → approve message). Requires `MNEMONIC` env var with a funded Cosmos wallet.

Build with the matching feature flag for the target network (`devnet-amplifier`, `stagenet`, `testnet`).

## Test ITS

```bash
axe test its
```

Deploys an interchain token locally, deploys it remotely to a destination chain via the ITS Hub, then sends a cross-chain transfer and verifies the balance on the destination. Relays through the full Amplifier pipeline (verify → vote → route → execute on hub).

# Load Tests

The `axe test load-test` command sends cross-chain transactions through the Axelar Amplifier pipeline and verifies them end-to-end. It supports two modes: **burst** (send N transactions as fast as possible) and **sustained** (send at a fixed TPS rate for a fixed duration).

## Modes

### Burst mode (default)

Send a fixed number of transactions all at once. All keys are funded upfront and all transactions are fired in parallel.

```bash
axe test load-test --num-txs 50 ...
```

### Sustained mode

Send transactions at a controlled rate for a set duration. Use `--tps` and `--duration-secs` together:

```bash
axe test load-test --tps 10 --duration-secs 300 ...
# sends 10 tx/s for 5 minutes = 3000 transactions total
```

**How it works:**

- A pool of `tps × key_cycle` wallets is derived and funded upfront (e.g. 30 wallets for 10 TPS with a 3s cycle).
- Each second, `tps` transactions are fired using the next batch of keys from the pool.
- Keys rotate on a configurable cycle (default 3 seconds): second 1 uses keys 0–9, second 2 uses keys 10–19, second 3 uses keys 20–29, second 4 reuses keys 0–9, etc. This ensures each key has time for its previous transaction to land before its nonce is reused.
- Use `--key-cycle N` to control the cycle length. Higher values use more wallets and reduce per-address mempool pressure on chains with aggressive mempool limits (e.g. `--key-cycle 6` doubles the wallet pool).
- **Concurrent send + verify:** In sustained mode, the Amplifier verification pipeline starts immediately as transactions confirm — it does not wait for the send phase to finish. Both phases run concurrently with live progress on separate lines:
  ```
  \ [42/300s]  fired: 420/3000  src-confirmed: 410  failed: 2  (target: 10 tx/s)
  - voted: 350/410  routed: 280/410  approved: 120/410  executed: 80/410
  ```
- The final summary shows end-to-end latency (avg/min/max), throughput, per-phase step and cumulative timing, pipeline counts, and any stuck transactions.
- A JSON report is written to `axe-load-test-logs/axe-load-test-<timestamp>.json` after each run for post-mortem analysis.

**Protocols supported in sustained mode:** GMP and ITS, both directions (Sol → EVM and EVM → Sol).

**ITS note:** Token deployment happens once upfront (cached across runs). Each pool key is pre-funded with enough tokens for its share of the total transfers before the send phase begins.

---

**GMP (default):** Sends ABI-encoded string payloads via `callContract` through a deployed `SenderReceiver` contract. The contract address is cached after first deploy and reused across runs.

**ITS (`--protocol its`):** Deploys an interchain token on the source chain, deploys the remote counterpart on the destination via the ITS Hub, then sends `InterchainTransfer` transactions. Supports both EVM → Sol and Sol → EVM directions.

**Verification:** Polling covers the full pipeline: `voted → routed → approved → executed` (GMP), or `voted → hub-approved → second-leg discovery → routed → approved → executed` (ITS). In sustained mode, verification runs concurrently with sending. In burst mode, it runs after all sends complete. Inactivity timeout is 200 seconds — the poller resets the timeout each time any transaction makes progress.

## Burst examples

### GMP: SOL → EVM

```bash
axe test load-test \
  --source-chain solana-18 \
  --destination-chain avalanche-fuji \
  --config ../axelar-contract-deployments/axelar-chains-config/info/devnet-amplifier.json
```

### GMP: EVM → SOL

```bash
axe test load-test \
  --source-chain avalanche-fuji \
  --destination-chain solana-18 \
  --num-txs 50 \
  --config ../axelar-contract-deployments/axelar-chains-config/info/devnet-amplifier.json
```

### ITS: SOL → EVM

```bash
axe test load-test \
  --source-chain solana-18 \
  --destination-chain avalanche-fuji \
  --protocol its \
  --config ../axelar-contract-deployments/axelar-chains-config/info/devnet-amplifier.json
```

### ITS: EVM → SOL

```bash
axe test load-test \
  --source-chain avalanche-fuji \
  --destination-chain solana-18 \
  --protocol its \
  --config ../axelar-contract-deployments/axelar-chains-config/info/devnet-amplifier.json
```

## Sustained examples

### GMP: 10 tx/s for 5 minutes, SOL → EVM

```bash
axe test load-test \
  --source-chain solana-18 \
  --destination-chain avalanche-fuji \
  --tps 10 \
  --duration-secs 300 \
  --config ../axelar-contract-deployments/axelar-chains-config/info/devnet-amplifier.json
```

### GMP: 5 tx/s for 2 minutes, EVM → SOL

```bash
axe test load-test \
  --source-chain avalanche-fuji \
  --destination-chain solana-18 \
  --tps 5 \
  --duration-secs 120 \
  --config ../axelar-contract-deployments/axelar-chains-config/info/devnet-amplifier.json
```

### ITS: sustained EVM → SOL

```bash
axe test load-test \
  --source-chain avalanche-fuji \
  --destination-chain solana-18 \
  --protocol its \
  --tps 3 \
  --duration-secs 180 \
  --config ../axelar-contract-deployments/axelar-chains-config/info/devnet-amplifier.json
```

## Stagenet / testnet / mainnet

On stagenet/testnet/mainnet the relayer requires gas payment. Build with the appropriate feature flag:

```bash
# Build (use cargo build, not cargo install — see note below)
cargo build --no-default-features --features stagenet
cp target/debug/axe ~/.cargo/bin/axe

# Burst
axe test load-test \
  --source-chain flow \
  --destination-chain solana-stagenet-3 \
  --num-txs 100 \
  --config ../axelar-contract-deployments/axelar-chains-config/info/stagenet.json

# Sustained with larger wallet pool for Flow
EVM_PRIVATE_KEY=0x... axe test load-test \
  --source-chain flow \
  --destination-chain solana-stagenet-3 \
  --tps 2 \
  --duration-secs 120 \
  --key-cycle 6 \
  --config ../axelar-contract-deployments/axelar-chains-config/info/stagenet.json \
  --source-rpc https://your-flow-rpc-endpoint

# ITS sustained
axe test load-test \
  --source-chain flow \
  --destination-chain solana-stagenet-3 \
  --protocol its \
  --tps 3 \
  --duration-secs 120 \
  --config ../axelar-contract-deployments/axelar-chains-config/info/stagenet.json
```

> **Note:** `cargo install --path .` does a clean compile which triggers a known borsh derive bug in `solana-axelar-std`. Use `cargo build` (incremental) + manual copy instead.

Run `axe test load-test --help` for all options.

## Decode

### Decode calldata

```bash
axe decode calldata 0x0f4433d3...   # auto-detects function from 4-byte selector
axe decode calldata 0x00000000...   # auto-detects ITS payload type
```

Decodes EVM calldata against a built-in ABI database (Gateway, ITS, ITS Factory, GMP SDK). Recursively decodes nested bytes fields (multicall batches, ITS payloads inside GMP calls). Whitespace in hex input is stripped automatically.

### Decode transaction

```bash
axe decode tx 0xabc123...                        # auto-discovers configs from sibling repo
axe decode tx 0xabc123... --chain avalanche       # skip brute-forcing, target one chain
axe decode tx 0xabc123... --config path/to.json   # use a specific config file
```

Fetches a transaction by hash from all EVM chains in parallel, then decodes the calldata and all event logs. Chains configs are auto-discovered from the sibling `axelar-contract-deployments` repo (mainnet, testnet, stagenet, devnet-amplifier).

Set `ALCHEMY_TOKEN` to use Alchemy RPCs for supported chains (faster and more reliable than public RPCs):

```bash
export ALCHEMY_TOKEN=your_token_here
axe decode tx 0xabc123...
```

## Configuration

All config lives in `.env` — see [`.env.example`](.env.example) for the full template.

| Variable                                                     | Used by                                    |
| ------------------------------------------------------------ | ------------------------------------------ |
| `CHAIN`, `ENV`, chain metadata                               | `init`                                     |
| `DEPLOYER_PRIVATE_KEY`, `GATEWAY_DEPLOYER_PRIVATE_KEY`, etc. | `deploy`                                   |
| `MNEMONIC`                                                   | `test gmp`, `test its` (Amplifier routing) |
| `ITS_*` vars                                                 | `deploy` (ITS steps), `test its`           |
| `TARGET_JSON`                                                | all commands (reads chain config)          |
| `ALCHEMY_TOKEN` (optional)                                   | `decode tx` (archive RPCs)                 |
