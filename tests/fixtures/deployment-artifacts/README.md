# Robinhood deployment artifact fixtures

These public bytecode fixtures reproduce the Robinhood testnet `predeployCodehash`
values. Only `contractName`, `bytecode`, and `deployedBytecode` are retained.
The relative paths mirror the deployment repository so tests also exercise Axe's
artifact selection, including the legacy ConstAddressDeployer and ITS proxy.

Sources from `axelarnetwork/axelar-contract-deployments` at commit
`fd0fcf837815eee04e398a04bcc3a3bf8030b581` and its locked npm dependencies:

- `evm/legacy/ConstAddressDeployer.json`
- `@axelar-network/axelar-gmp-sdk-solidity@6.2.0`: Create3Deployer and Operators
- `@axelar-network/interchain-token-service@2.2.0`: InterchainProxy

Expected hashes were independently calculated with the deployment repository's
`getBytecodeHash(artifact, 'robinhood')`. That helper hashes `deployedBytecode`.
Creation bytecode is retained to catch accidentally hashing it instead.
These are artifact runtime templates, before constructor immutable substitutions.
