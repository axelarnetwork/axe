# Safe, resumable EVM deployments

`axe deploy run` uses this workflow on every network. No `--hardened` flag is
needed or accepted. Use the same binary for the testnet rehearsal and mainnet;
network endpoints, expected addresses, funding and chain parameters change,
but transaction recovery, approvals, governance batching and verifier checks do not.
`devnet-amplifier` retains direct Cosmos execution: the same execute messages
are signed by the configured governance wallet, journaled, approved and checked,
without proposal wrappers, deposits or voting waits. Preflight requires that
`MNEMONIC` control the on-chain governance authority for those calls. The named
proposal-wait checkpoints verify the recorded direct transactions on devnet.
A devnet journal created by a draft that submitted governance proposals must be
finished with its matching binary; axe will not reinterpret it as direct execution.

This flow supports new deployments with local EVM keys and Cosmos mnemonics.
Deployment execution refuses pre-journal state and authorities that the supplied signers
cannot control. There is no `--legacy` bypass, automatic state conversion or
`deploy reset` command. Deployment leaves unsupported state untouched; status and
smoke tests can still read it without a deployment plan. Hardware,
multisig and offline signing adapters are not implemented. A multisig **recipient**
is allowed where no further transaction from that recipient is required.

## Inputs and preflight

Keep credentials outside the repository (or in the existing ignored `.env`).
Use the existing deployment environment described in [deploying.md](deploying.md):
`CHAIN`, `CHAIN_NAME`, `CHAIN_ID`, `RPC_URL`, `TOKEN_SYMBOL`, `DECIMALS`, `ENV`,
`SALT`, `ITS_SALT`, `ITS_PROXY_SALT`, `TARGET_JSON`, `MNEMONIC` and all four EVM
role keys. Optionally supply `MULTISIG_PROVER_MNEMONIC` for a separate prover-admin
account; otherwise axe uses `MNEMONIC` for both roles. Axe derives the admin
address, displays it for approval and pins it in the saved public plan. No
`PROVER_ADMIN` environment variable is needed. A different admin account is
rejected on resume.

Verifier keys remain with their operators. Axe discovers the eligible set at activation; it does not register verifier support or change Router administration.
The service name is resolved from the network: `validators` on devnet-amplifier,
`amplifier` on mainnet, testnet and stagenet. Configuration, verifier queries and
postconditions use the same mapping. Preflight fails if the service cannot be queried.

Put the public deployment settings in the same `.env` as your existing deployment
configuration. No separate JSON plan is required. Amounts are integer strings in
base units, not whole tokens:

```dotenv
# CHAIN_ID above is the EVM chain ID.
AXELAR_CHAIN_ID=axelar-testnet-lisbon-3
GATEWAY_OWNER=0x...
OPERATORS_OWNER=0x...
GAS_SERVICE_OWNER=0x...
ITS_OWNER=0x...
FACTORY_OWNER=0x...
GATEWAY_OPERATOR=0x...
EVM_GAS_BUDGET=50000000000000000
COSMOS_FEE_BUDGET=100000000
REWARD_AMOUNT=100000000
VOTING_THRESHOLD=2/3
SIGNING_THRESHOLD=2/3
BLOCK_EXPIRY=50
CONFIRMATION_HEIGHT=1
```

No verifier roster is required in `.env`. `EXPECTED_INITIAL_VERIFIERS` is no
longer read and can be removed. At activation, axe discovers eligible verifiers
from the ServiceRegistry and displays network-specific names from its built-in
address book. Unmapped addresses are labeled **Unknown (not in axe's address
book)**; this is informational, not a protocol authorization decision. Verifier
operators still register chain support separately.

Initialization previews these settings and saves a public plan in deployment
state. Resume compares the `.env` settings against the saved plan and refuses
changes. Missing required settings are reported together. Credentials continue
to come from the environment and are excluded from saved state.

The optional `--plan /path/to/public-plan.json` flag remains supported for
existing JSON-based workflows and takes precedence over public `.env` plan
settings. A saved JSON deployment can also resume without either input; if you
supply public `.env` plan settings, supply the full set and keep it identical to
the saved plan.

Choose confirmation depth, poll expiry and thresholds for the target chain;
the values above are illustrative. The actual prover set must have at least the
service minimum number of verifiers, stay within any service maximum, have
valid distinct ECDSA keys and positive weights, and match the plan's signing
threshold. A single verifier is sufficient only when the service permits it.
There is no temporary single-verifier bootstrap.

Before the first transaction, axe checks every future role credential, chain
identity, support for the selected EVM confirmation policy, artifacts, Coordinator instantiation permissions,
protocol wiring, governance authorities, balances and service verifier limits. External verifier registrations may still be outstanding; their
completion is an explicit final activation dependency.

On governance networks, funding checks reserve one expedited proposal deposit, both remaining reward
payments and the remaining fee allowances. The three proposals are sequential:
a later batch can only proceed after the preceding batch passes, releasing its
deposit. A journaled deposit awaiting resolution is treated as already reserved.
Immediately before a new Cosmos submission, axe separately checks the actual
balance against that transaction’s payments and fee; it cannot spend an expected
refund that has not arrived.
Deposits and voting periods come from live governance parameters.
The live deposit is displayed separately from fees. Stale chains-config deposit
values cannot override the chain's requirement; a deposit is escrowed funding,
not the transaction fee. The flow only reuses it after a passed proposal and an
actual refund.
Reward-pool policy comes from `reward_pool_messages` in the checked-out axe code,
not the public plan: mainnet uses epoch duration `14845`, participation threshold
`8/10` and `3424660000` base units of rewards per epoch. The proposal preview
shows these values and postconditions check them. `REWARD_AMOUNT` is the initial
payment to each pool, not its ongoing per-epoch reward rate. Gas budgets
are enforced spending limits, not guarantees about future gas prices. A run
stops if a transaction would exceed its budget.

## Run, stop and resume

Build this checkout first and ensure `axe` resolves to this binary, rather than
an older installed release. Run from the repository's development environment:

```bash
cargo build --release --locked
export PATH="$PWD/target/release:$PATH"
```

Start a fresh rehearsal from the axe repository. Axe loads `.env` automatically;
there is no need to source it into your shell. Replace `unichain-sepolia` below
with your configured `CHAIN` when deploying a different chain:

```bash
axe --network testnet deploy run --axelar-id unichain-sepolia
```

The command initializes public state if needed. Each step explains its effects
and asks `y/n`. Each new transaction additionally displays its signer, target or
message, network and fee before approval. Answering `n`, EOF or noninteractive
input stops the flow; dependent steps are not skipped.
`init` and `run` check for an interactive terminal before initialization or RPC
access and explain how to proceed if stdin is redirected. There is no unattended
approval bypass. Status can be run without a terminal.

The original command had four proposal submissions. Registration now includes
reward-pool creation, reducing the flow to three expedited proposal batches:

1. Instantiate the Cosmos Gateway, VotingVerifier and MultisigProver.
2. Register the deployment and create both reward pools in the same atomic
   governance execution.
3. Register the ITS edge on the ITS Hub.

Each proposal pause names the proposal to vote on. On testnet it prints
`bash scripts/vote_testnet_proposal.sh "YOUR_VALIDATOR_NAMESPACE" <proposal-id>`:
select your testnet kubectl context and replace the namespace placeholder before
running it from the axe repo. The script votes Yes from each matching validator
pod; axe only prints the command. Mainnet pauses instead direct you to coordinate
with validator operators. Casting a vote does not end the voting period; resume
after the proposal reaches PASSED. Only the applicable resume command is shown,
using the executable you launched. `--activate` is added at the verifier checkpoint
and retained for subsequent steps, not offered during the first two batches.

Deployment pauses and errors end with a handoff showing the network and chain,
next unfinished step, saved proposal ID when applicable, and why axe stopped.
The continue command and read-only status command appear on separate lines for
copying. Finish the stated action before continuing; axe does not monitor in
the background after it exits. Recovery flags mentioned in an error must be
added to the continue command. Keep the state and journal after errors: a lost
connection does not prove that a submitted transaction failed.

Axe exits after submitting each proposal. Resume checks the recorded proposal
once; pending governance exits again instead of waiting for the voting period.
Rejected, failed or changed proposals stop the flow without creating replacements.

```bash
axe --network testnet deploy status --axelar-id unichain-sepolia
axe --network testnet deploy run --axelar-id unichain-sepolia
```

Status shows proposal status, voting end time and current tally. Add `--votes`
to include individual voter records across all pages. Voting and verifier daemon rollout remain external operations.

The hardened flow uses the original deployment actions and dependency order.
After the first two governance batches and reward funding, it stops at the
verifier checkpoint. The EVM gateway has not been deployed yet. Configure the
external verifier workers and register chain support, then continue with:

```bash
axe --network testnet deploy run --axelar-id unichain-sepolia --activate
```

On testnet, the checkpoint prints the infrastructure PR paths and a chain
configuration block populated with the deployed Multisig, MultisigProver and
VotingVerifier addresses. Add the chain to the applicable ampd deployments under
`config_toml.grpc.blockchain_service.chains` and configure its EVM handler using
the current infra image/version. Keep authenticated RPC URLs in the approved
secret mechanism. After the PR is merged and deployed, confirm the verifier
workers are healthy, then run from the axe repo with the testnet kubectl context:

```bash
bash scripts/register_chain_support.sh unichain-sepolia
```

This existing script registers ECDSA public keys and chain support for workers
0–21 in `testnet-amplifiers`; include that fleet in the rollout. Axe prints these
instructions but does not open the PR, deploy infrastructure or run the script.
Mainnet guidance instead asks the external verifier operators to complete their
rollout and registrations. Then run the activation command above.

If the registry reports `not enough verifiers`, axe pauses and repeats the setup
instructions. Eligible verifiers must be authorized, sufficiently bonded, and
registered for this chain. The contract does not return the exact count below
its minimum. This is a readiness condition, not an LCD outage; no prover
initialization transaction is submitted. Other query failures remain errors.

This displays eligible registered verifiers and checks the service count limits.
If the prover has no current set, axe simulates and submits the original
`update_verifier_set` action with transaction approval. Simulation catches
insufficient keyed participants before broadcast. The registry can select a
subset, and the prover can omit participants without registered keys, so axe
reads and validates the **actual resulting prover set**. It displays identities,
known/unknown names, EVM signer addresses, weights, total weight, threshold and
set ID/hash, then asks approval before saving the gateway signer snapshot.
Declining leaves the existing prover transaction in the journal; resuming
reviews the current set without reinitializing it. Extra eligible verifiers and
unknown local names do not block approval when the set satisfies service limits.
The gateway uses the approved set in its original proxy constructor. It then
finishes the original Operators, gas-service, ownership-transfer and ITS steps,
and submits the third governance batch for ITS registration. No Router freezing,
pause/unpause calls, gateway upgrades or additional authority-transfer calls are
introduced. `--activate` releases the local verifier checkpoint; it does not
perform a separate on-chain activation operation. Axe refuses `--activate` before
the preceding steps have completed, including on the first run.

The existing gateway/operator/gas-service ownership transfers use the configured
recipients. The initial ITS/factory owners and gateway operator are parameters
of their original deployment transactions, avoiding additional transfers.
ITS operatorship retains the original deployer default.

Keep the gateway deployer wallet unused from `PredictGatewayAddress` until its
gateway transactions finish. Cosmos configuration pins its future CREATE
address. Axe checks the nonce and refuses to send a gateway deployment if that
prediction has changed; it does not silently substitute a different address.
On an interrupted transaction, it reuses the journaled nonce and signed bytes.

On testnet, register support with the verifier workers you control. The same
minimum-count and operator-review flow applies on mainnet. Before creating a new
gateway proxy transaction, axe rechecks the current prover set. If it changed,
axe shows the new set and requires fresh approval before replacing the snapshot.
Once proxy bytes are journaled, its signer snapshot cannot change; recovery
reuses those bytes. These reads cannot lock the prover against a concurrent
external update. Old journals retain any obsolete roster metadata only to keep
their existing fingerprints resumable; it no longer restricts discovery. Later
checks compare the gateway’s historic epoch-1 hash with this snapshot; changes
to live registry membership do not block the remaining deployment. A later
gateway rotation meeting the selected confirmation policy is displayed and
requires approval. Its epoch and hash are recorded for subsequent checks.

## Node trust requirements

**Mainnet deployment requires operator-controlled Axelar LCD/Tendermint and EVM
RPC nodes, including any archival recovery endpoint.** Secure access to those
nodes and verify their network configuration independently before deployment.
Axe trusts their transaction results, account sequences, balances, contract state,
proposal contents, canonical EVM block hashes and (when selected) `finalized`
responses. It does not verify consensus,
light-client proofs or validator signatures for those responses. A server can
fake `node_info.network`; the chain-ID check catches configuration mistakes and
does not authenticate an archival endpoint. A dishonest node can therefore
invalidate the safety assumptions behind recovery and postcondition checks.

Safe deployment disables implicit public-node fallback, including preflight.
Public testnet endpoints can be used for rehearsal, but that does not validate
the mainnet node trust setup.

## Recovery contract

State is stored under the OS data directory:
`axe/deployments/<network>/<chain>/state.json`, beside `journal.json` and a process
lock. The folder also contains `inputs.json` and a rebuildable `artifacts/` cache. On Linux this normally starts at `~/.local/share/`. Back up the entire
folder and the deployment configuration together. Treat the journal as sensitive:
it contains approved signed transactions, although it contains no private keys
or mnemonics.

A signed transaction is durably journaled **before** broadcasting. Recovery looks
up that transaction and may rebroadcast the identical signed bytes. Previously
approved transactions may therefore be broadcast on resume without another
approval. It never creates another proposal or reward payment merely because a
response was lost.

Cosmos journal entries retain the account sequence and confirmed height, outcome
and response (including proposal events). Confirmed entries are read locally on
resume. When an unconfirmed transaction is missing, axe compares the live account
sequence before rebroadcasting. An advanced sequence is **not** evidence of
success. To retrieve missing evidence, append `--recovery-lcd <archival-LCD-URL>`
to the usual resume command. Axe checks the endpoint’s reported Axelar chain ID
and uses it only for reads. This is not cryptographic proof of the network.
If neither trusted endpoint can establish what
happened, the flow stops; unknown reward payments must never be recreated.

A Cosmos transaction rejected at CheckTx has no confirmed failure receipt. For
insufficient fees, resume with the exact recorded action and a higher **total**
fee in the configured denomination’s base units:

```bash
axe --network testnet deploy run --axelar-id unichain-sepolia \
  --bump-fees 'InstantiateChainContracts/cosmos' --cosmos-fee 200000
```

The amount above is illustrative; choose it from the node’s required fee and your
approved fee budget. Axe first reconciles every same-sequence attempt and checks
that the live account sequence is unchanged. It verifies the original signature
against the signer, chain and account number, then changes only the fee amount.
Messages, memo, timeout, signer, sequence, gas limit and fee denomination remain
unchanged. The operator sees the replacement and approves it before persistence
and broadcast. Every attempted hash remains recoverable; at most one can consume
that account sequence. The budget reserves the maximum fee at each sequence,
plus fees from attempts at other sequences. This also applies to a proposal
submission rejected before inclusion; it does not recreate an included proposal.
After an interrupted replacement, use the ordinary resume command without the
fee-replacement flags to reconcile or broadcast the already approved bytes.

This path does not override transaction-size limits, change gas limits, or promise
to evict a transaction already accepted by a Cosmos mempool. For node-local size
or policy rejection, diagnose the configured operator-controlled node and, if
appropriate, repair it or point the deployment configuration to another controlled
node on the same network, then resume the identical journaled transaction.
`--recovery-lcd` remains read-only. A protocol-wide size/validity limit or an
unresolved consumed sequence still requires investigation; do not edit or delete
the journal to force progress. `--retry-failed` is not a CheckTx recovery option.

A confirmed failed Cosmos execution can be retired with
`--retry-failed 'StepName/cosmos'`. Supported steps are `WaitForVerifierSet`,
`AddRewards` and the three batch submissions. For batch submissions, this means
the **submission transaction failed at DeliverTx and created no proposal**.
A proposal that was created and subsequently failed or was rejected is never
recreated. Axe requires recorded failure at a nonzero height, an advanced account
sequence and no saved proposal ID. An ante-handler failure that did not consume
the sequence requires account reconciliation; it is not retired automatically.

For a reverted EVM action, use its exact journal key, for example:

```bash
axe --network testnet deploy run --axelar-id unichain-sepolia \
  --retry-failed 'AxelarGasService/gas implementation' --retry-gas-limit 5000000
```

The gas limit is illustrative and optional; set it only after diagnosing the
failure. Recovery requires a canonical **finalized failed receipt**, even when
normal deployment uses one confirmation. Unknown or successful transactions
cannot be retired. Only the next pending step is eligible. Retrying gateway
CREATE transactions is refused: their consumed nonce changes the address already
registered on Cosmos. Preserve the journal and reconcile that registration
through a separately reviewed recovery; there is no automatic address override.
Other CREATE retries are refused if that signer already has later signed actions.

Axe displays the failed hash and asks approval to retire it, then exits. Resume
without `--retry-failed` and `--retry-gas-limit`; the optional gas override is saved.
The pending step creates a new transaction with an advanced nonce/sequence and
its own approval, preserving the execution intent. Historical attempts and their
fee liability remain in the journal. This also works after interruption between
retirement and the fresh attempt; never remove the failed entry by hand.

EVM receipts must be successful, canonical and meet the selected confirmation
policy. New deployments default to **1 confirmation**, meaning inclusion in a
block. Use `--evm-confirmations 2` to wait for one additional block, any positive
count for a deeper wait, or `--evm-confirmations finalized` to require the RPC's
finalized head to cover the receipt. The same setting is available through
`EVM_CONFIRMATIONS` in `.env`; the CLI flag takes precedence. This is independent
of `CONFIRMATION_HEIGHT`, which configures verifier voting on cross-chain events.

The policy is saved in the journal and inherited on resume unless a CLI/env
value overrides it. Journals created before this option preserve their original
`finalized` policy; pass `--evm-confirmations 1` to explicitly change it. Gateway
state checks and RPC preflight follow the same selected policy.

A confirmation count does **not** establish consensus finality. On resume, axe
rechecks cached receipts against canonical block hashes. If a receipt was
reorganized, it resolves the original transaction hash again; it never allocates
a new nonce to compensate. An unexplained consumed nonce stops the run. These
checks detect a reorg when observed; they cannot prevent one or undo dependent
Cosmos actions already submitted.

Axe waits up to 30 minutes for inclusion and the selected confirmation target;
Ctrl+C is safe throughout. Set
`--evm-wait-seconds <seconds>` to change this bound (`0` pauses immediately when
not ready). A timeout prints resume instructions. Governance votes still cause
immediate pause-and-exit, regardless of this setting.

For a dropped or underpriced EVM transaction, append
`--bump-fees 'StepName/transaction label'`, using the exact action printed by axe.
Axe displays the original hash, nonce and replacement gas liability for approval.
The replacement must retain the signer, chain, nonce, destination, value, gas
limit, transaction type and calldata. Only fees increase, within the saved budget
and current balance. Signed replacement bytes are saved before broadcast, and
all previous hashes are retained: either the original or a replacement can win.
Fee replacement allocates no new nonce. An unexplained consumed nonce stops the
deployment; a proven revert uses the separate `--retry-failed` procedure above.

Completed steps are checked against saved contract code, implementation,
authority and initial signer evidence, plus Cosmos configuration and registration
postconditions. The fingerprint pins signer identities, addresses, deployment
parameters and artifact bytecode, rather than whole artifact JSON or mutable
protocol metadata. A protocol code migration at the same address is shown with
old/new code IDs and checksums for explicit approval, after wiring and authority
checks pass. This approval cannot override changed addresses, authorities,
deployment artifacts or configuration. EVM intent hashes use a versioned field
encoding rather than Alloy’s JSON serialization.

Before broadcasting, axe stores the approved EVM ABI/creation/runtime artifacts
and the three per-chain Cosmos store-code hashes in `inputs.json`, whose hash is
pinned in the journal. Resume reconstructs the artifact cache from that snapshot
and uses the pinned Cosmos hashes. Updating the deployments checkout or running
`npm install` therefore cannot change those deployment inputs. Keep the live
chains-config addresses and deployment output intact; this snapshot is not a
backup of mutable on-chain state or the axe binary.

Existing version-2 journals without a snapshot bootstrap one only after their
original fingerprint matches. Restore their original artifact bytecode and
Cosmos code hashes first if those sources have already changed. Missing or edited
snapshots fail closed; restore the matching backup. There is no blanket
fingerprint bypass or implicit approval of different deployment code. Other
public settings must still match; changed fields are named when a snapshot exists.

The journal format is version 2. Draft version-1 journals are not automatically
migrated; preserve them and their matching binary if an earlier draft has already
submitted transactions. Do not delete a journal to start over.

A matching unused legacy state file (no completed steps, proposals or deployment
addresses) no longer obscures a current namespaced journal. Axe prints a notice
and leaves the legacy file intact. Conflicting histories still stop with both
paths listed for explicit inspection and archival.

Configuration writes preserve existing permissions and symlinks. Configuration
locks live under axe’s data directory, outside the sibling configuration checkout.
Deployment also rejects transaction sends if a spawned task loses its required
session; it cannot silently fall back to unjournaled sending.

Only one operator process should own a deployment. File locks protect processes
sharing this state/configuration; they are not a distributed lock across copies
on different machines. Do not reset, hand-edit progress, discard a journal or
start another deployment directory to bypass a failure. Restore backups or
investigate the recorded transaction/proposal first. Adoption of pre-journal
deployments and replacement of rejected proposals are unsupported.

## Rehearsal acceptance

**This acceptance checklist has not been executed end to end on live testnet.**
Offline tests do not establish mainnet readiness.

Before using this on mainnet, run a fresh testnet deployment and record evidence:

1. Omit or mismatch each future signer and verify no transaction is submitted.
2. Decline a step and a transaction, then resume; verify the action remains pending.
3. Close the process during a broadcast and immediately after confirmation.
   Resume and check that hashes, addresses, proposal IDs and reward payments are unchanged.
4. Close after each of the three proposal submissions. Resume while voting is
   pending and again after passage; record votes and verified postconditions.
5. Reach the verifier checkpoint with registrations outstanding. Confirm the
   EVM gateway is not deployed and no freeze/pause/upgrade writes were submitted.
6. Attempt `--activate` below the service minimum and with invalid/duplicate keys
   or a wrong threshold; confirm no gateway is sent. Try additional eligible
   verifiers and an unknown local name; review their actual prover set and approve.
   Decline once and resume without repeating initialization. Change the prover set
   before proxy signing and verify fresh approval is required; after journaling
   proxy bytes verify the snapshot is immutable. Check the gateway constructor's
   exact approved signer hash and configured operator.
7. Confirm all final owners, finish the third governance batch, then resume a
   completed run and confirm it submits nothing. Separately authorize and run
   GMP and ITS traffic in both directions, including verifier participation.
8. Simulate a missing Cosmos tx-index response after confirmation: resume must
   use saved evidence. For an unconfirmed entry with a consumed sequence, confirm
   it stops without broadcasting; resolve a pruned receipt through the archival
   LCD option. Test the explicit retry path with a confirmed failed non-proposal
   action and a failed proposal-submission transaction; verify successful payments
   and existing proposals cannot be retried. Retire a finalized EVM revert,
   interrupt before its fresh attempt and resume; verify the old hash and fee
   remain, the new nonce advances, and gateway CREATE retry is refused.
9. Exercise Cosmos CheckTx fee rejection and same-sequence replacement, including
   interruption after saving replacement bytes and the original attempt confirming
   first. Exercise EVM fee replacement, numeric confirmation depths and delayed finality.
   Reorganize a cached EVM receipt and verify resume reconciles the original hash. Check that the replacement
   retains its nonce and call, and that either attempted hash can confirm.
   Interrupt after saving the replacement and resume. Approve an authorized
   gateway rotation during the remaining deployment and verify the saved initial
   signer hash remains the deployment reference.
10. Between batches, update the artifact/code-hash sources and verify resume uses
    the pinned snapshot. Corrupt or remove the snapshot and verify a clean stop.
    Rehearse devnet direct execution separately: no governance queries, proposals
    or deposits, unchanged execute messages, and the same crash recovery.
11. Audit the transaction journal against the original deploy actions. The only
   governance change is batching the existing reward-pool creation messages with
   chain registration. Verify a consumed predicted gateway nonce causes a stop.

Offline Rust regression tests cover journal persistence, policy/step drift,
proposal payload matching, key stripping, locking, sequence safety, cached
confirmations, replacement execution invariants, original-transaction wins,
delayed finality, lost task context, verifier snapshots, LCD balance request paths
and Cosmos CheckTx fee recovery. Passing these tests does not replace the live
testnet rehearsal above.
