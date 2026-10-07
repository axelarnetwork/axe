# Deploying a new chain (`axe deploy`)

`axe deploy run` uses preflight checks, interactive approvals, transaction
journaling and resumable governance waits on every network. See the
[deployment and recovery guide](hardened-deploy.md) for the full workflow.
Testnet and mainnet use the same code; there is no `--hardened` flag.

New deployments wait for **1 EVM confirmation** (the inclusion block). Use
`axe deploy run --evm-confirmations 2` for one additional block, or
`axe deploy run --evm-confirmations finalized` for RPC finality. You can also set
`EVM_CONFIRMATIONS` in `.env`. The policy is saved for resumes; existing journals
without this setting retain `finalized` until explicitly overridden. Numeric
confirmations are faster but can reorg; resume rechecks canonical receipt hashes.
This setting does not change verifier voting's `CONFIRMATION_HEIGHT`.

`axe deploy` writes back into the chains-config and reads contract artifacts,
so it needs a real checkout — set it up as a sibling directory:

```bash
# 1. Clone the contract deployments repo as a sibling
git clone https://github.com/axelarnetwork/axelar-contract-deployments.git
cd axelar-contract-deployments && npm install && cd ..

# 2. Configure
cp axe/.env.example axe/.env
# Edit .env with your chain details, keys, and mnemonics

# 3. Initialize and deploy
axe deploy init         # optional: run also initializes when needed
axe deploy run
```

`init` requires every deployment role key: `DEPLOYER_PRIVATE_KEY`,
`GATEWAY_DEPLOYER_PRIVATE_KEY`, `GAS_SERVICE_DEPLOYER_PRIVATE_KEY`, and
`ITS_DEPLOYER_PRIVATE_KEY`. It also requires `MNEMONIC`, `SALT`, `ITS_SALT`,
and `ITS_PROXY_SALT`, alongside the chain metadata and public deployment settings
in `.env.example` (owners, thresholds and budgets).
It reports missing variables together and validates credentials before
writing the chain config or deployment state.

The prover admin address is derived from `MULTISIG_PROVER_MNEMONIC`, or from
`MNEMONIC` when no separate admin mnemonic is supplied. All role credentials are
checked before transactions and must remain available on resume. New deployments
save public state and a transaction journal; credentials are loaded from the
environment. No `EXPECTED_INITIAL_VERIFIERS` list is required. At the verifier
checkpoint, resume with `--activate` when enough eligible verifiers are ready.
Axe shows known names and unknown addresses from the network's address book,
validates the actual prover set against service limits and signing requirements,
and asks approval before pinning it for gateway deployment. Unknown names are
informational, not an automatic rejection. Verifier rollout, support registration
and proposal voting remain external operations.

`init` refuses to overwrite existing deployment state. Resume with `deploy run`;
do not reset or delete the journal to bypass a failure. New deployments require
the full public plan and reject per-run key, artifact and salt overrides.

Pre-journal state is unsupported for deployment execution, including state with
no completed steps. `deploy run` rejects it and leaves the files untouched. There is no
`--legacy` bypass or `deploy reset` command. Preserve old records and reconcile
any on-chain actions before attempting a new deployment. Current journaled
runs continue to support resume and explicit recovery.

`deploy status` and the `test gmp` / `test its` commands can still read old state;
they do not require a deployment plan. The state-based smoke tests need only
`DEPLOYER_PRIVATE_KEY` (or `EVM_PRIVATE_KEY`) and `MNEMONIC` for their EVM and
Cosmos relay transactions. Axe loads `.env` automatically and reports missing or
invalid credentials before contacting the chain. Existing credentials in old
state remain readable, but subsequent state saves strip credentials, including
when a GMP test caches its SenderReceiver address. Keep credentials in `.env`
or the environment for future runs. Status requires no credentials.

`deploy init` and `deploy run` require interactive stdin for transaction and
trust-change approvals. Read-only and local steps do not prompt. These commands
reject redirected or piped stdin before initialization or RPC access. Run directly
in a terminal, or allocate one with `ssh -t` remotely.
If the RPC does not expose `eth_syncing` (JSON-RPC `-32601`, including that
response wrapped in HTTP 403), axe warns that sync status is unknown and continues
to the contract tests, with approval before each new transaction. A syncing node,
stale blocks, unrelated HTTP errors and failed deployment-critical checks still block the run.
There is no blanket compatibility-check bypass.
Devnet uses the `validators` ServiceRegistry service; mainnet, testnet and
stagenet use `amplifier`. Preflight queries that service before any transaction.

Cosmos instantiation derives its salt from `Coordinator:<chain>:<SALT>` so
different chains can use the same version label. Both `init` and `run` check
that the Coordinator can instantiate the selected CosmWasm code IDs before
sending deployment transactions. Instantiation checks permission and address
availability again immediately before submission. The Coordinator is the
creator, so allowing only the governance module or proposer is insufficient.
Missing permissions require a separate governance update to the code's
instantiate allowlist, preserving existing addresses. Axe never automatically
replaces a failed proposal.

Deployment does not guarantee three automatic broadcast attempts. An unresolved
broadcast pauses the flow; resume with `axe deploy run` using the existing state
and journal. Axe checks the recorded transaction outcome before any rebroadcast
and reuses the signed bytes unless you explicitly approve a fee-only replacement.
A lost response never authorizes a fresh transaction with a new nonce or sequence.

`axe test gmp --axelar-id <chain>` waits for the source transaction's block to
be covered by the RPC's `finalized` block before requesting verification. L2
finality can take tens of minutes even when a receipt appears immediately.
The wait shows finalized/required heights, remaining blocks, elapsed time, and
a progress bar measuring how much of the initial block gap has closed. An
approximate ETA appears after at least 30 seconds of observations with finalized
height advancing. It is hidden after two minutes without advancement because
finality can arrive in batches. The wait times out after one hour without
requesting verification. A missing or changed receipt also stops verification.
This avoids asking handlers to vote on an unfinalized transaction, which they
would report as `NotFound`.

```
workspace/
├── axe/
└── axelar-contract-deployments/
```

## Commands

```bash
axe deploy run          # checks, approves, journals, and runs pending steps
axe deploy status       # shows progress and proposal tallies
axe deploy status --votes # also lists individual voters
```
