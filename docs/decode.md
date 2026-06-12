# Decoding (`axe decode`)

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

### Solana program activity

```bash
axe decode sol-activity --program gateway --network devnet-amplifier --limit 5
axe decode sol-activity --network testnet                              # all programs on testnet
axe decode sol-activity --program its --network devnet-amplifier --json # machine-readable JSON
```

Shows recent transactions for Axelar Solana programs (Gateway, ITS, GasService, Memo). Auto-discovers program addresses from the sibling `axelar-contract-deployments` config files. Decodes instruction names, args, and CPI events. Use `--json` for structured output consumable by LLMs for debugging.

### EVM contract activity

```bash
axe decode evm-activity --contract gateway --network devnet-amplifier --chain avalanche-fuji --limit 5
axe decode evm-activity --contract its --network testnet --chain flow --json
```

Shows recent events from Axelar EVM contracts (Gateway, ITS, GasService) using `eth_getLogs`. Auto-discovers contract addresses from config files. Decodes event names and parameters using the embedded ABI database.

### Debugging Cross-Chain Messages

See [axelar-debugging.md](axelar-debugging.md) for a guide to tracing GMP and ITS messages through source, Axelar, and destination-chain state.
