# Governance proposals (`axe propose`)

```bash
# operator fast-path (default), submit + monitor only (no relay)
axe propose testnet berachain --op pause

# full round-trip: submit → vote → relay → execute
axe propose testnet berachain --op unpause --relay

# time-lock instead of the operator fast-path
axe propose testnet berachain --op pause --type timelock --relay

# any other call — pass the target and abi-encoded calldata directly
axe propose testnet berachain \
  --target 0xB5FB4BE02232B1bBA4dC8f81dc24C26980dE9e3C \
  --calldata 0x9f409d77... --relay
```

`axe propose` submits an `AxelarServiceGovernance` (ASG) proposal to an edge
chain's governance contract via an Axelar cosmos gov proposal that calls
`AxelarnetGateway.call_contract`, prints the vote action
(`./vote_<env>_proposal.sh <env>-nodes <id>`), and monitors the proposal to a
terminal status. With `--relay` it then delivers the GMP to the edge chain and
executes it — the second leg the amplifier relayer often skips for
gov-originated messages: `construct_proof` → `submitProof` → `ASG.execute` →
`executeOperatorProposal` (operator), or wait-for-eta → `executeProposal`
(time-lock).

### Catalog operations (`--op`)

| `--op`           | Target  | Call                              |
| ---------------- | ------- | --------------------------------- |
| `pause`          | gateway | `setPauseStatus(true)`            |
| `unpause`        | gateway | `setPauseStatus(false)`           |
| `set-trusted`    | ITS     | `setTrustedChain(--its-chain)`    |
| `remove-trusted` | ITS     | `removeTrustedChain(--its-chain)` |
| `its-pause`      | ITS     | `setPauseStatus(true)`            |

The ITS ops assume the **hub-model** ITS (`setTrustedChain`/`setPauseStatus`);
for a legacy ITS (e.g. v2.1.1's `setTrustedAddress`), pass the call directly
with `--target` and `--calldata`. Omitting `--op` requires both `--target` and
`--calldata`.

### Flags & defaults

| Flag                | Default              | Notes                                                       |
| ------------------- | -------------------- | ----------------------------------------------------------- |
| `--type`            | `operator`           | `operator` fast-path or `timelock`                          |
| `--relay`           | off                  | relay to the edge chain + execute after the vote passes     |
| `--standard`        | off (expedited)      | submit a standard (1h) gov proposal instead of expedited    |
| `--eta <unix>`      | now + ASG delay + 5m | time-lock activation time                                   |
| `--its-chain <x>`   | -                    | required for `set-trusted`/`remove-trusted`                 |
| `--y` / `--yes`     | off                  | skip the confirmation prompt                                |
| `--confirm-mainnet` | off                  | required to run against mainnet (otherwise refused)         |

Env: `MNEMONIC` — any funded Axelar account (pays the deposit, refunded on
pass); `EVM_GOVERNANCE_OPERATOR_KEY` (preferred) or `EVM_PRIVATE_KEY` — the
edge-chain key for `--relay` (the operator fast-path's final execute requires
the ASG `operator` key; if the key isn't the operator, the proposal is relayed +
approved and a `cast` command is printed for the operator to finish).

Before submitting, `axe propose` verifies the target is a real, correctly-wired
ASG (code present, `governanceAddress` == the gov module), runs an idempotency
check, and shows a review block: the target contract is labelled
(e.g. `berachain gateway`) — or flagged **`Unknown Destination`** in red — the
raw calldata is decoded with the same engine as `axe decode` (or flagged
**`Unknown Calldata`** in red), and the deposit is shown in AXL.
