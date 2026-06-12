# Monitoring (`axe verifiers`, `axe verifier-votes`, `axe its-ownership`)

### List active verifiers for a chain

```bash
axe verifiers mainnet solana
axe verifiers testnet xrpl --json
```

Lists the verifier set currently authorised to vote on the given chain's `VotingVerifier`, with a known-name lookup (e.g. "Inter Blockchain Services") where available. Networks: `devnet-amplifier`, `stagenet`, `testnet`, `mainnet`.

### Show a verifier's recent votes

```bash
axe verifier-votes mainnet solana axelar1s2cf963rm0u6kxgker95dh5urmq0utqq3rezdn
axe verifier-votes mainnet solana axelar1... --limit 50 --json
```

Walks `wasm-voted` events for the given voter on a chain's `VotingVerifier` and prints each poll they participated in along with the vote value: `Y` (succeeded), `F` (failed), `?` (not_found). Useful for diagnosing a single verifier's behaviour after a stuck poll. Networks: `testnet`, `mainnet`.

To investigate a *specific* poll's full participation (who voted, who skipped), query the contract directly:

```bash
axelard q wasm contract-state smart <voting-verifier-addr> '{"poll":{"poll_id":"<id>"}}' \
  --node <axelar-rpc> --chain-id <axelar-chain-id> -o json | jq
```

The `participation` map shows each verifier and whether they voted; the `tallies` show the aggregate. To get the actual vote value per voter on a specific poll, fetch the voter's `wasm-voted` tx by hash with `axelard q tx <hash>`.

## ITS ownership

```bash
axe its-ownership devnet-amplifier
axe its-ownership stagenet
axe its-ownership testnet
axe its-ownership mainnet
axe its-ownership stagenet --json
```

Reads the network's chains-config (see [Config resolution](../Readme.md#configuration))
and prints a compact table of ITS owner/operator addresses. EVM rows query
`owner()` and verify configured candidates with `isOperator(address)`; Sui,
Solana, and Stellar are included where their config/RPC data exposes the same
fields. The Owner Type column makes the ITS owner explicit as `gov: <contract>`,
`EOA`, `contract`, `account`, or `-`; the Gov column shows deployed
governance contract addresses. The summary still counts governance deployments
and owner matches. Address cells use terminal hyperlinks when the chain config
has an explorer URL. Use `--json` for full uncompressed addresses, explorer
URLs, owner type, query sources, and governance contract details.
