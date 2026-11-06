# Governance proposals (`axe propose`)

For ASG deployment and the separate gateway/ITS ownership transfer, see
[Service governance deployment and ownership handoff](asg-handoff.md).

```bash
# operator fast-path (default), submit + monitor only (no relay)
axe propose testnet avalanche --op pause

# full round-trip: submit → vote → relay → execute
axe propose testnet avalanche --op unpause --relay

# time-lock instead of the operator fast-path
axe propose testnet avalanche --op pause --type timelock --relay

# one proposal to make every eligible edge chain trust the hub's Ethereum entry
axe propose mainnet --op set-trusted --its-chain ethereum --chains all --confirm-mainnet

# explicit destinations, or use --skip <a,b> to exclude keys from --chains all
axe propose testnet --op set-trusted --its-chain solana --chains avalanche,flow --relay

# any other call — pass the target and abi-encoded calldata directly
axe propose testnet avalanche \
  --target 0xB5FB4BE02232B1bBA4dC8f81dc24C26980dE9e3C \
  --calldata 0x9f409d77... --relay
```

`axe propose` submits an `AxelarServiceGovernance` (ASG) proposal to an edge
chain's governance contract via an Axelar cosmos gov proposal that calls
`AxelarnetGateway.call_contract`, prints the vote action
(`./scripts/vote_<env>_proposal.sh <env>-nodes <id>`), and monitors the proposal to a
terminal status. With `--relay` it then delivers the GMP to the edge chain and
executes it — the second leg the amplifier relayer often skips for
gov-originated messages: `construct_proof` → `submitProof` → `ASG.execute` →
`executeOperatorProposal` (operator), or wait-for-eta → `executeProposal`
(time-lock).

### Recovering an interrupted run

Rerun the same command after a connection failure. Before submitting, `axe`
searches the hub's proposal history for the same governance route, proposal
type, target and calldata. It resumes the newest matching proposal and preserves
its original payload (including a time-lock ETA), without spending another deposit.
If history cannot be read, it refuses to submit. A rejected or failed match is
reported rather than automatically replaced.

Before a new submission, `axe` creates an empty marker under its local data
directory (`axe/proposals/`). If the process loses the broadcast response before
the proposal becomes visible in history, the marker prevents a duplicate. It
contains no signing material. An uncertain submission remains blocked until it
appears in history or the user verifies failure and explicitly starts a new one.

Use `--proposal-id <id>` with the original operation or target/calldata to select
a particular proposal; a mismatch fails before any transaction. To intentionally
repeat an operation as a new proposal, use `--new-proposal`. Remove that flag
when recovering its interrupted run.

Read-only governance HTTP requests retry temporary transport failures, HTTP 5xx,
408 and 429 responses with bounded backoff. Transaction broadcasts are not blindly
retried. Relay recovery finds the governance message at the proposal's voting
deadline, checks gateway approval/consumption and the remaining ASG approval or
schedule, and skips completed steps. A consumed message with no remaining ASG
approval/schedule is reported as executed or cancelled, without replaying it.
Old proposals require an RPC that retains their block results.

The marker also prevents concurrent submissions sharing the same local data
directory. It cannot coordinate different machines or data directories.

### Catalog operations (`--op`)

| `--op`           | Target  | Call                              |
| ---------------- | ------- | --------------------------------- |
| `pause`          | gateway | `setPauseStatus(true)`            |
| `unpause`        | gateway | `setPauseStatus(false)`           |
| `set-trusted`    | ITS     | `setTrustedChain(--its-chain)`    |
| `remove-trusted` | ITS     | `removeTrustedChain(--its-chain)` |
| `its-pause`      | ITS     | `setPauseStatus(true)`            |

Trust operations resolve `--its-chain` against the live ITS Hub registry,
case-insensitively, and encode the registry's exact spelling (for example,
`ethereum` becomes `Ethereum` on mainnet). Unknown or ambiguous names stop
proposal preparation. Each edge ITS is probed for its trust API: ITS 2.2 uses
`setTrustedChain`/`removeTrustedChain`; ITS 2.1 uses
`setTrustedAddress(chain, "hub")`/`removeTrustedAddress`. Only an explicit
contract revert permits the legacy probe; transport, RPC and decoding failures
stop preparation. Omitting `--op` requires both `--target` and `--calldata`.

### Multi-chain proposals

`--chains all` starts from the live ITS Hub registry and excludes frozen,
deprecated, non-EVM and unconfigured chains, the chain being trusted, and any
config keys named by `--skip`. `--chains a,b` selects explicit config keys.
Each selected edge is verified before submitting one Cosmos proposal containing
one governance call per edge. Raw calldata is single-chain only.

When the plan has more than one edge, all ASGs must share one operator and
`EVM_GOVERNANCE_OPERATOR_KEY` must derive that operator's address, even without
`--relay` or with `--type timelock`. A missing or mismatched key stops preparation.
Each destination's exact routing name is checked against the amplifier Router
or consensus nexus registry. With `--relay`, amplifier chains use proof
construction and consensus chains wait for validator approval before execution.

### Flags & defaults

| Flag                | Default              | Notes                                                       |
| ------------------- | -------------------- | ----------------------------------------------------------- |
| `--type`            | `operator`           | `operator` fast-path or `timelock`                          |
| `--relay`           | off                  | relay to the edge chain + execute after the vote passes     |
| `--chains <all\|a,b>` | -                  | select destinations instead of the positional chain; multiple edges require `--op` |
| `--skip <a,b>`      | -                    | config keys to exclude from `--chains all`                  |
| `--mnemonic-env <name>` | `MNEMONIC`       | select the mnemonic variable for Cosmos proposal and relay proof signing |
| `--proposal-id <id>`| auto-discover        | resume a matching existing proposal; never submit           |
| `--new-proposal`    | off                  | intentionally submit again instead of recovering history    |
| `--standard`        | off (expedited)      | submit a standard (1h) gov proposal instead of expedited    |
| `--eta <unix>`      | now + ASG delay + 5m | time-lock activation time                                   |
| `--its-chain <x>`   | -                    | required for trust operations; resolved to the hub registry's exact spelling |
| `--y` / `--yes`     | off                  | skip submission confirmation off mainnet; ignored on mainnet |
| `--confirm-mainnet` | off                  | required to run against mainnet (otherwise refused)         |

Mainnet always requires interactive review and approval; redirected stdin and
`--y` cannot bypass it. Agents must also obtain explicit user approval of the
exact final title, description, metadata and complete payload before signing
or submitting a proposal on any network, as required by `AGENTS.md`.

Env: `MNEMONIC` — any funded Axelar account (pays the deposit, refunded on
pass); `EVM_GOVERNANCE_OPERATOR_KEY` (preferred) or `EVM_PRIVATE_KEY` — the
edge-chain key for `--relay` (the operator fast-path's final execute requires
the ASG `operator` key; if the key isn't the operator, the proposal is relayed +
approved and a `cast` command is printed for the operator to finish).

Network selection does not select a different mnemonic automatically. To use
a separate testnet proposer without changing the mainnet `MNEMONIC`, set
`TESTNET_PROPOSER_MNEMONIC` and pass `--mnemonic-env TESTNET_PROPOSER_MNEMONIC`.
The argument is an environment variable name, never the mnemonic itself.
The command prints the selected variable and derived public address. Missing,
empty or invalid selected values stop execution without falling back to
`MNEMONIC`. The same selection pays Cosmos proof-construction fees during
`--relay`, including proposal recovery. Keep the flag when rerunning.
The existing `.env` still overrides shell values for identical variable names.

On mainnet, if the signing wallet cannot cover the deposit plus the 10 AXL
fee buffer, `axe` automatically checks its existing CPI multisig authorization
for `/cosmos.gov.v1.MsgSubmitProposal`. If the grant is active and CPI has the
full deposit available, `axe` warns before confirmation that CPI will pay the
deposit. The review names both the proposer/deposit payer and signer/fee payer.
No flag is needed. A sufficiently funded signing wallet continues to pay its
own deposit.

The fallback wraps the proposal in authz `MsgExec`, with CPI multisig
(`axelar14vps3ev03zyp2wmj89etx8rrxdxyltfy4rzl5m`) as proposer and the wallet
from the selected mnemonic variable as grantee. That wallet still needs at least 10 AXL for fees;
any deposit refund returns to CPI. The grant authorizes proposal submission,
not a bank transfer or fee sponsorship. Missing, expired or unreadable grants,
insufficient CPI funds, and insufficient signer gas stop submission. This
fallback only applies to `axe propose` on `axelar-dojo-1`.

Before submitting, `axe propose` verifies the target is a real, correctly-wired
ASG (code present, `governanceAddress` == the gov module), runs an idempotency
check, and shows the exact proposal title, description, and complete payload
as decoded JSON before asking for approval. The JSON is decoded from the
prepared protobuf and includes metadata, proposer, deposits, execution authority,
contract addresses, funds and nested contract messages. The same prepared proposal is submitted
after confirmation. Proposal text contains no tool branding. In the review
block, the target contract is labelled
(e.g. `avalanche gateway`) — or flagged **`Unknown Destination`** in red — the
raw calldata is decoded with the same engine as `axe decode` (or flagged
**`Unknown Calldata`** in red), and the deposit is shown in AXL.
