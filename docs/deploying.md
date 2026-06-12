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

```
workspace/
├── axe/
└── axelar-contract-deployments/
```

## Commands

```bash
axe deploy run          # runs all 23 steps sequentially
axe deploy status       # shows progress
axe deploy reset        # start over
```
