# Deploying a new chain (`axe deploy`)

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
axe deploy init
axe deploy run
```

`init` requires every deployment role key: `DEPLOYER_PRIVATE_KEY`,
`GATEWAY_DEPLOYER_PRIVATE_KEY`, `GAS_SERVICE_DEPLOYER_PRIVATE_KEY`, and
`ITS_DEPLOYER_PRIVATE_KEY`. It also requires `MNEMONIC`, `SALT`, `ITS_SALT`,
and `ITS_PROXY_SALT`, alongside the chain metadata in `.env.example`.
It reports missing variables together and validates credentials before
writing the chain config or deployment state.

Before starting, axe requires a mnemonic that derives the prover admin address.
Set `MULTISIG_PROVER_MNEMONIC` for that wallet, or omit it only if `MNEMONIC`
already derives the same address. Axe checks the planned admin for new contracts
and reads the on-chain admin for an existing prover. Invalid or mismatched keys
stop deployment before transactions. A mnemonic cannot be recovered from an
address, and importing a key into axelard does not make it available to axe.
Once verifiers are ready, axe sends `update_verifier_set` automatically and
checks the on-chain admin again immediately before doing so. Resumes past the
completed verifier-set step do not require this credential. Infrastructure
rollout, verifier registration, and proposal voting remain manual steps.
An explicitly supplied `MULTISIG_PROVER_MNEMONIC` replaces the saved admin
mnemonic only after preflight succeeds, allowing a wrong saved key to be repaired.

`init` refuses to overwrite existing deployment state. To repair an incomplete
setup, export the missing role keys or ITS salts and rerun `axe deploy run`.
The run command loads missing role keys and salts without replacing saved values or resetting
completed steps, then validates the configuration before sending transactions.
Use `axe deploy reset` only when intentionally starting over.

Cosmos instantiation derives its salt from `Coordinator:<chain>:<SALT>` so
different chains can use the same version label. Before submitting a proposal,
axe checks for an existing deployment and verifies its addresses, code, admin,
source gateway, and prover identity. A matching deployment is reused, including
legacy deployments that used the unscoped salt. Pending proposals are reused.
If an earlier instantiation proposal failed with an address collision, rerunning
`deploy run` rechecks that step while preserving earlier completed steps. It
checks address availability before submitting a replacement proposal.

Both `init` and `run` check that the Coordinator can instantiate the selected
CosmWasm code IDs before saving state or sending deployment transactions.
Instantiation checks permission again immediately before submission. The
Coordinator is the creator of these contracts, so allowing only the governance
module or proposer wallet is insufficient. Missing permissions require a
governance update to the code's instantiate allowlist, preserving existing
addresses. After that update passes, `deploy run` can recover an instantiation
proposal that failed with `can not instantiate: unauthorized`.

ITS deployment and the EVM GMP smoke test retry transient RPC requests up to
three times. Broadcast retries reuse identical signed transaction bytes, so a
lost response does not cause a second transaction with a new nonce. Contract
reverts are not retried. If requests still fail, resume deployment using its
existing state so the predicted contract addresses are checked again.

`axe test gmp --axelar-id <chain>` waits for the source transaction's block to
be covered by the RPC's `finalized` block before requesting verification. L2
finality can take tens of minutes even when a receipt appears immediately.
The wait shows finalized/required heights and times out after one hour without
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
axe deploy run          # runs all 24 steps sequentially
axe deploy status       # shows progress
axe deploy reset        # start over
```
