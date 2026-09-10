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
writing the chain config or deployment state. `MULTISIG_PROVER_MNEMONIC`
is optional: without it, the pipeline waits for a manual verifier-set update.

`init` refuses to overwrite existing deployment state. To repair an incomplete
setup, export the missing role keys or ITS salts and rerun `axe deploy run`.
The run command loads missing values without replacing saved values or resetting
completed steps, then validates the configuration before sending transactions.
Use `axe deploy reset` only when intentionally starting over.

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
