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

# 3. Install
cd axe
cargo install --path .

# 4. Initialize and deploy
axe init
axe deploy
```

```
workspace/
├── axe/
└── axelar-contract-deployments/
```

## Commands

| Command              | Description                                   | Relayer |
| -------------------- | --------------------------------------------- | ------- |
| `axe init`           | Initialize a new chain deployment from `.env` | -       |
| `axe deploy`         | Run all deployment steps sequentially         | -       |
| `axe status`         | Show deployment progress                      | -       |
| `axe reset`          | Reset all steps to pending                    | -       |
| `axe test gmp`       | End-to-end GMP loopback test                  | no      |
| `axe test its`       | Deploy + transfer an interchain token         | no      |
| `axe test load-test` | Cross-chain load test                         | yes     |
| `axe decode calldata`| Decode raw EVM calldata                       | -       |
| `axe decode tx`      | Fetch & decode a full EVM transaction          | -       |

## Deploy

```bash
axe deploy              # runs all 23 steps sequentially
axe status              # shows progress
axe reset               # start over
```

## Test GMP

```bash
axe test gmp
```

Sends a loopback GMP message and relays it through the full Amplifier pipeline end-to-end.

## Test ITS

```bash
axe test its
```

Deploys an interchain token locally, deploys it remotely to a destination chain via the ITS Hub, then sends a cross-chain transfer and verifies the balance on the destination. Relays through the full Amplifier pipeline (verify → vote → route → execute on hub).

# Load Tests
The axe test load-test command sends cross-chain transactions through the Axelar amplifier pipeline and verifies them end-to-end.

**GMP (default):** Sol → EVM derives N independent Solana keypairs, funds them, then sends all N CallContract transactions in parallel. EVM → Sol sends N callContract transactions from a single EVM signer with a 200ms stagger.

**ITS (`--protocol its`):** Deploys an interchain token on the source chain, deploys the remote counterpart on the destination, then sends N InterchainTransfer transactions through the ITS Hub. Supports both EVM → Sol and Sol → EVM directions.

**Verification:** After all transactions are submitted, polling covers the full pipeline: voted → routed → approved → executed (GMP), or voted → hub-approved → second-leg discovery → routed → approved → executed (ITS). Results are printed as a timing summary and saved to report.json.

## Load Test (SOL -> EVM)

```bash
axe test load-test \
  --source-chain solana-18 \
  --destination-chain avalanche-fuji \
  --config ../axelar-contract-deployments/axelar-chains-config/info/devnet-amplifier.json
```

## Load Test (EVM -> SOL)

```bash
axe test load-test \
  --source-chain avalanche-fuji \
  --destination-chain solana-18 \
  --config ../axelar-contract-deployments/axelar-chains-config/info/devnet-amplifier.json
```

Override the number of transactions (default: 5):

```bash
axe test load-test \
  --source-chain avalanche-fuji \
  --destination-chain solana-18 \
  --config ../axelar-contract-deployments/axelar-chains-config/info/devnet-amplifier.json \
  --num-txs 50 \
  --source-rpc https://example.avalanche.fuji.rpc.com
```

## Load Test ITS (SOL -> EVM)

```bash
axe test load-test \
  --source-chain solana-18 \
  --destination-chain avalanche-fuji \
  --protocol its \
  --config ../axelar-contract-deployments/axelar-chains-config/info/devnet-amplifier.json
```

## Load Test ITS (EVM -> SOL)

```bash
axe test load-test \
  --source-chain avalanche-fuji \
  --destination-chain solana-18 \
  --protocol its \
  --config ../axelar-contract-deployments/axelar-chains-config/info/devnet-amplifier.json
```

## Load Test (stagenet)

On stagenet/testnet/mainnet the relayer requires gas payment. Build with the appropriate feature flag:

```bash
cargo install --path . --no-default-features --features stagenet
axe test load-test \
  --source-chain solana-stagenet-3 \
  --destination-chain flow \
  --config ../axelar-contract-deployments/axelar-chains-config/info/stagenet.json \
  --num-txs 100
```

## Load Test ITS (stagenet)

```bash
cargo install --path . --no-default-features --features stagenet
axe test load-test \
  --source-chain solana-stagenet-3 \
  --destination-chain flow \
  --protocol its \
  --config ../axelar-contract-deployments/axelar-chains-config/info/stagenet.json \
  --num-txs 100
```

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
