# Service governance deployment and ownership handoff

`its-trust` has two additional subcommands for amplifier EVM chains. They are
separate from the existing `deploy` stages and do not change their behavior.
Both commands preview by default, without loading a signing key or writing
configuration. They require a local `axelar-contract-deployments` checkout.

```bash
# Preview the CREATE3 deployment, including complete constructor calldata.
axe its-trust testnet robinhood deploy-asg --from <deployer-address>

# Apply after reviewing the full transaction and explicitly answering y.
# Load ASG_DEPLOYER_PRIVATE_KEY from a secure location outside the repository.
axe its-trust testnet robinhood deploy-asg --from <deployer-address> --apply

# Preview both ownership transfers and the operational roles being retained.
axe its-trust testnet robinhood handoff-asg --from <current-owner-address>

# Approve each ownership transaction separately.
axe its-trust testnet robinhood handoff-asg --from <current-owner-address> --apply
```

Mainnet writes additionally require `--confirm-mainnet`. There is no `--yes`
option, and redirected stdin cannot approve writes. `deploy-asg` requires
`ASG_DEPLOYER_PRIVATE_KEY` deriving the network's established deployer, and
`--from` must be that address. `handoff-asg` uses `ASG_PRIVATE_KEY` matching
`--from`, the current gateway/ITS owner. The signing key is only needed for a
new transaction, not for previews or recovery of a recorded transaction.

Every `--apply` also requires `EVM_GOVERNANCE_OPERATOR_KEY`, matching the
network's ASG operator below. This verifies access to the operational key
before deployment or handoff. It does not sign the ownership transactions.
There is no fallback to generic wallet variables. Previews need neither key.
Load keys from a secure location outside the repository or the existing ignored
`.env`. Do not paste keys into command arguments, logs, or chat.

## Network settings and roles

| Setting | Testnet | Mainnet |
| --- | --- | --- |
| ASG deployer | `0x81e63eA8F64FEdB9858EB6E2176B431FBd10d1eC` | `0xba76c6980428A0b10CFC5d8ccb61949677A61233` |
| CREATE3 salt key | `v6.1.0` | `v6.0.4-axelar` |
| Required ASG address | `0xb55A09E9fde39755c39EF9A7911431Ba0c96332A` | `0x7Acbae6CBa67d78AAf69e47000884aE00F9B2525` |
| ASG operator | `0x6e079CD1c6bBb72680DeDF2687d711AE9427eE8e` | `0xc8a8399c3D1207f4e109673be7047604737c1D56` |
| Minimum timelock | 300 seconds | 259200 seconds |
| Governance chain | `axelar` | `axelar` |
| Governance address | `axelar10d07y265gmmuvt4z0w9aw880jnsr700j7v9daj` | Same |

The handoff transfers **gateway ownership and ITS ownership only**. The gateway
operator and ITS operator roles remain unchanged. ASG's own operator is a
different role: executing an operator proposal requires prior authorization
through Axelar governance.

ASG has no EOA owner key. Axelar governance controls it through authenticated
GMP. The testnet/mainnet operator addresses and timelocks above were also
checked against live deployments. Arc testnet (`arc-8`, EVM chain 5042002)
already has ASG `0xb55A09E9fde39755c39EF9A7911431Ba0c96332A` owning its
gateway and ITS. Treat that deployment as existing state, not a fresh handoff.

Read-only mainnet checks on 2026-11-02 found this separation on Hedera,
Hyperliquid, Monad and XRPL EVM: gateway and ITS owner
`0x7Acbae6CBa67d78AAf69e47000884aE00F9B2525`, gateway operator
`0xb8cd93c83a974649d76b1c19f311f639e62272bc`, and
`ITS.isOperator(ASG) == false`. The command retains the target chain's actual
gateway operator rather than assigning that mainnet address on every network.
ITS has role membership queries, not a single enumerable operator getter.
The review reports whether the sender and ASG hold that role, without claiming
to enumerate every ITS operator.

Legacy consensus gateways use governance/mint-limiter roles. These commands
reject them rather than applying amplifier ownership semantics.

## Deployment and verification

ASG uses the checkout's GMP SDK `AxelarServiceGovernance` artifact and the
canonical CREATE3 factory `0x6513Aedb4D1593BA12e50644401D976aebDc90d8`.
The salt defaults to the network policy above. An explicit `--salt` must match
that policy. Wrong senders, signing keys, salts, or predicted addresses stop
deployment rather than producing a different ASG address.

The CREATE3 factory address, helper bytecode, sender and salt determine the
ASG address. A chain-specific factory gives a different result even with the
right signing key. The command verifies the canonical factory's runtime and
queries its prediction, requires the established network ASG address, and
prints that predicted address immediately before the transaction confirmation
(including rebroadcast confirmation). If the canonical factory is absent,
establish it first. This command does not bootstrap factories or silently use
another factory. Existing chain deployment factory records are preserved, and
the factory used for ASG is recorded in its `create3Deployer` field.

`--config` selects a local chains-config JSON. Otherwise `CHAINS_CONFIG` or the
sibling checkout is used. `--artifact` selects the exact ASG artifact when an
existing deployment uses a different SDK build. `handoff-asg --asg <address>`
can verify and record an existing ASG missing from config. An explicit address
conflicting with an existing config record is rejected.

Before deployment, the command verifies the RPC chain ID, gateway/ITS wiring,
and CREATE3 runtime against the checkout artifact. Before handoff, it checks
ASG's complete runtime, gateway, governance strings and hashes, operator, and
timelock. Constructor simulation supplies runtime with its immutable values
filled in. An RPC that cannot provide finalized state or simulate creation
causes the command to stop.

The handoff checks both current owners before sending either transaction.
The sender must own each contract that still needs a transfer. If the owner
is a multisig or another contract, use the displayed transaction through that
owner's authorization process, then rerun to verify and record the result.
ITS ownership moves first, followed by gateway ownership. Each transaction is
simulated and shown with the complete input, sender, chain ID, target, value,
nonce and maximum gas cost before approval.

## Recovery and configuration

After approval, signed transaction bytes are recorded under the local data
directory's `asg-transactions/` before broadcast. Rerun the same command after
an interruption. Recovery checks the original receipt or offers to rebroadcast
the exact recorded bytes. It does not allocate another nonce automatically.
A sender/chain lock spans nonce selection, signing, broadcast and finality,
including recovery. This serializes ASG sends across targets and config files
using the same local data directory. External wallets and other machines must
still avoid concurrent use of that sender.
A reverted transaction or consumed nonce without its receipt stops recovery
for investigation. Do not delete a journal merely because a send timed out.

Each successful transaction must finalize and satisfy its on-chain
postconditions before its result is written to
`axelar-contract-deployments/axelar-chains-config/info/<network>.json`.
Config writes are atomic, retain unrelated fields, and reject changes made
during review. A partial handoff records the completed transfer, and reruns
skip ownership already held by ASG. Verified existing deployments can be
recorded without a new deployment transaction.

The commands modify the local configuration only. They do not commit or push
the deployments repository.

## Robinhood testnet rehearsal

Live checks on 2026-11-02 found no ASG configured for Robinhood testnet (EVM
chain ID 46630). Its gateway and ITS are owned by
`0x49845e5d9985d8dc941462293ed38EEfF18B0eAE`. The gateway operator is
`0x81e63eA8F64FEdB9858EB6E2176B431FBd10d1eC`, which is also the deployer
recorded for the existing Hedera and Arc testnet ASGs. Recheck the live roles
in the previews before approving any changes.

**Prerequisite:** the canonical CREATE3 factory has no code on Robinhood
testnet as of that check. Establish it through the existing infrastructure
deployment process before these deployment commands can succeed. The earlier
prediction `0x87b9A22fC2d6b6dEaB3FfacCe3b5c6D252CEF9B8` used Robinhood's
chain-specific factory and does not meet the shared-address requirement.

1. Set `EVM_GOVERNANCE_OPERATOR_KEY` to the testnet ASG operator key for
   `0x6e079CD1c6bBb72680DeDF2687d711AE9427eE8e`. For deployment, set
   `ASG_DEPLOYER_PRIVATE_KEY` to the deployer key for
   `0x81e63eA8F64FEdB9858EB6E2176B431FBd10d1eC`.
2. Preview, then deploy after reviewing the exact transaction:

   ```bash
   ./target/release/axe its-trust testnet robinhood deploy-asg \
     --from 0x81e63eA8F64FEdB9858EB6E2176B431FBd10d1eC

   ./target/release/axe its-trust testnet robinhood deploy-asg \
     --from 0x81e63eA8F64FEdB9858EB6E2176B431FBd10d1eC --apply
   ```

   With the canonical CREATE3 factory and testnet salt, the required ASG is
   `0xb55A09E9fde39755c39EF9A7911431Ba0c96332A`. Gateway and ITS ownership
   remain unchanged at this stage.
3. Before handing over ownership, test governance delivery and operator
   execution with the gateway's `owner()` getter (`0x8da5cb5b`). This creates
   a real testnet governance proposal and ASG execution, but does not change
   gateway state. It requires a funded Cosmos proposer (`MNEMONIC`), validator
   votes, and native gas on the ASG operator account. That operator had zero
   native balance on Robinhood when checked. Review and approve the exact
   title, description and payload displayed by the command:

   ```bash
   ./target/release/axe propose testnet robinhood --type operator \
     --target 0x2a949565682ad89ca4Ad325499C36d3865a3ee02 \
     --calldata 0x8da5cb5b --relay
   ```

4. After successful execution, set `ASG_PRIVATE_KEY` to the current owner
   key for `0x49845e5d9985d8dc941462293ed38EEfF18B0eAE`. Keep the ASG
   operator key set. Preview and then approve each ownership transfer:

   ```bash
   ./target/release/axe its-trust testnet robinhood handoff-asg \
     --from 0x49845e5d9985d8dc941462293ed38EEfF18B0eAE

   ./target/release/axe its-trust testnet robinhood handoff-asg \
     --from 0x49845e5d9985d8dc941462293ed38EEfF18B0eAE --apply
   ```

5. Rerun the handoff preview to verify both owners are ASG and the operational
   roles are retained. Follow with the normal GMP/ITS smoke tests before
   treating the rehearsal as complete.

The public Robinhood RPC intermittently returned `historical state ... is
not available` for finalized-state queries during preparation. Use an RPC
that can serve finalized state in the selected local chains config. The
command stops on unavailable finalized state rather than weakening its checks.
The CLI loads the existing `.env` with precedence over shell exports, so
update any conflicting signer entries there when switching roles.

## Separate deploy-flow slice

The existing deploy steps stay unchanged. A subsequent slice, tested on
Robinhood, will append:

1. Trust every eligible peer locally, using `its-trust`.
2. Make remote EVM ITS contracts trust the new chain through the existing
   governance proposal flow, reviewing the exact title, description and payload.
3. Handle each supported, active non-EVM chain individually, including Solana,
   Sui and Stellar, with chain-specific authorization and verification.
4. Deploy/verify ASG and transfer gateway and ITS ownership to it.

Those stages must reconcile live state and existing proposal evidence before
submitting anything. In particular, Arc's already submitted remote-EVM trust
proposal must be discovered or attached and monitored, not submitted again.
ASG ownership handoff does not itself establish or verify the trust matrix.
