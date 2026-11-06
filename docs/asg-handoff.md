# Service governance deployment and ownership handoff

`axe asg` has two subcommands for amplifier EVM chains. They are
separate from the existing `deploy` stages and do not change their behavior.
Both commands preview by default, without loading a signing key or writing
configuration. They require a local `axelar-contract-deployments` checkout.

```bash
# Preview the CREATE3 deployment, including complete constructor calldata.
axe asg testnet robinhood deploy-asg --from <deployer-address>

# Apply after reviewing the full transaction and explicitly answering y.
# Load ASG_DEPLOYER_PRIVATE_KEY from a secure location outside the repository.
axe asg testnet robinhood deploy-asg --from <deployer-address> --apply

# Preview both ownership transfers and the operational roles being retained.
axe asg testnet robinhood handoff-asg --from <current-owner-address>

# Approve each ownership transaction separately.
axe asg testnet robinhood handoff-asg --from <current-owner-address> --apply
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
| ASG deployer | `0x81e63eA8F64FEdB9858EB6E2176B431FBd10d1eC` | `0xB1209B087e544785138784A50933d163A9087a7D` |
| CREATE3 salt key | `v6.1.0` | `v6.0.4-axelar` |
| Existing shared ASG (discovery only) | `0xb55A09E9fde39755c39EF9A7911431Ba0c96332A` | `0x7Acbae6CBa67d78AAf69e47000884aE00F9B2525` |
| ASG operator | `0x6e079CD1c6bBb72680DeDF2687d711AE9427eE8e` | `0xc8a8399c3D1207f4e109673be7047604737c1D56` |
| Minimum timelock | 300 seconds | 259200 seconds |
| Governance chain | `axelar` | `axelar` |
| Governance address | `axelar10d07y265gmmuvt4z0w9aw880jnsr700j7v9daj` | Same |

The former mainnet deployer `0xba76c6980428A0b10CFC5d8ccb61949677A61233`
was reported compromised and must not be used. New mainnet ASG deployments use
the deployer recorded for Arc and Robinhood above. This changes predicted CREATE3
addresses; regenerate previews before approving any deployment. Existing contract
addresses and historical deployment provenance are not rewritten.

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

The deployment recipe is the same for every supported chain:

1. Use the network's ASG deployer, salt, operator, governance source and
   timelock in the table above. These settings are enforced by the command.
2. Use the chain's existing `Create3Deployer.address`. If an ASG record already
   specifies `create3Deployer`, that recorded factory takes precedence.
3. Verify the factory's latest and finalized runtime against the checkout's
   GMP SDK artifact. Ask that factory for the ASG address and show it alongside
   the factory, signing account, ASG operator, gateway, ITS, governance source,
   timelock, salt and full deployment payload before confirmation.
4. Deploy with `ASG_DEPLOYER_PRIVATE_KEY`, wait for two confirmations (the
   inclusion block plus one successor), verify ASG at a confirmed block and
   record its actual address and factory in the chain's ASG config record.
   Deployment and ownership handoff both use two confirmations, not finality.

**The policy is canonical per network, but the resulting address is per chain.**
Different factories produce different ASG addresses even with the same sender
and salt. The shared ASG addresses in the table are recognized for existing
installations, not imposed on new deployments. No Interop factory keys are
required. The command does not deploy factories: a chain missing its configured
factory must first complete the normal chain deployment process using its
available deployment accounts.

ASG uses the checkout's GMP SDK `AxelarServiceGovernance` artifact. An explicit
`--salt` must match the network policy. Missing or incorrect signing keys,
wrong senders, unavailable factories, unexpected factory runtime, and a
prediction conflicting with a configured ASG stop deployment.

Existing configured ASGs are verified and reused, even if the chain's current
factory would predict a different address. The established shared ASG is also
checked when no configured deployment exists, so older chains with incomplete
config do not silently receive a replacement. A conflict requires reconciling
the config. Reuse updates only observed contract properties and preserves
existing deployment provenance. It does not infer a factory or deployment
transaction from the current chain config.

Fresh deployments print the actual predicted ASG and factory again immediately
before signing or rebroadcast confirmation. After approval, factory runtime and
prediction are rechecked. Existing chain factory records remain unchanged, and
the factory actually used is recorded in ASG's `create3Deployer` field.
Governance proposals and ownership handoff must use that chain's recorded ASG
address, never assume the old shared address.

`--config` selects a local chains-config JSON. Otherwise `CHAINS_CONFIG` or the
sibling checkout is used. `--artifact` selects the exact ASG artifact when an
existing deployment uses a different SDK build. `handoff-asg --asg <address>`
can verify and record an existing ASG missing from config. An explicit address
conflicting with an existing config record is rejected.

Before deployment, the command verifies the RPC chain ID, gateway/ITS wiring,
and CREATE3 runtime against the checkout artifact. Before handoff, it checks
ASG's complete runtime, gateway, governance strings and hashes, operator, and
timelock. Constructor simulation supplies runtime with its immutable values
filled in. Handoff verifies ASG and ownership at a block with two confirmations,
pinned by canonical hash and rechecked for reorgs. Deployment factory checks still
require finalized state. An RPC that cannot serve the required state or simulate
creation causes the command to stop.

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
A sender/chain lock spans nonce selection, signing, broadcast and confirmation waiting,
including recovery. This serializes ASG sends across targets and config files
using the same local data directory. External wallets and other machines must
still avoid concurrent use of that sender.
If a different call reaches the same sender/target journal, recovery first
checks the saved transaction. A successful transaction can be retired only
after its receipt has two confirmations and matches the canonical block.
The command shows that evidence and asks to archive the record, then stops;
rerun to re-read deployment/ownership and prepare the remaining work. The
archive preserves the signed bytes and terminal evidence. Recovery of the
original successful call still reuses its receipt.

Confirmed reverts and nonces consumed at a confirmed block without an available
receipt also offer evidence-based archival before a rerun. Pending transactions,
RPC failures and unconfirmed or noncanonical receipts remain blocked. Do not
delete a journal merely because a send timed out.

Deployment and ownership transfers wait for two confirmations (the inclusion
block plus one successor) and recheck the receipt and canonical inclusion block.
Their postconditions are checked at a block with the same depth, pinned by
canonical hash. Existing ASG reuse and handoff recovery use that same depth.
This is not finality; later reorgs remain possible. Both operations must satisfy their
on-chain postconditions before their results are written to
`axelar-contract-deployments/axelar-chains-config/info/<network>.json`.
Config writes are atomic, retain unrelated fields, and reject changes made
during review. A partial handoff records the completed transfer, and reruns
skip ownership already held by ASG. Verified existing deployments can be
recorded without a new deployment transaction.

The commands modify the local configuration only. They do not commit or push
the deployments repository.

## Robinhood testnet rehearsal

The Robinhood testnet rehearsal (EVM chain ID 46630) deployed and verified ASG
on 2026-11-02. Its gateway and ITS are still owned by
`0x49845e5d9985d8dc941462293ed38EEfF18B0eAE`. The gateway operator is
`0x81e63eA8F64FEdB9858EB6E2176B431FBd10d1eC`, which is also the deployer
recorded for the existing Hedera and Arc testnet ASGs. Recheck the live roles
in the previews before approving any changes.

Robinhood's existing CREATE3 factory is
`0x4A0fc57047cc99Cb28152730A42F5a432Aece667`. With the testnet deployer and
salt above, it predicts ASG `0x87b9A22fC2d6b6dEaB3FfacCe3b5c6D252CEF9B8`.
This per-chain address is the intended deployment. The old shared factory
`0x6513Aedb4D1593BA12e50644401D976aebDc90d8` is not needed.

1. Set `EVM_GOVERNANCE_OPERATOR_KEY` to the testnet ASG operator key for
   `0x6e079CD1c6bBb72680DeDF2687d711AE9427eE8e`. For deployment, set
   `ASG_DEPLOYER_PRIVATE_KEY` to the deployer key for
   `0x81e63eA8F64FEdB9858EB6E2176B431FBd10d1eC`.
2. Preview, then deploy after reviewing the exact transaction:

   ```bash
   ./target/release/axe asg testnet robinhood deploy-asg \
     --from 0x81e63eA8F64FEdB9858EB6E2176B431FBd10d1eC

   ./target/release/axe asg testnet robinhood deploy-asg \
     --from 0x81e63eA8F64FEdB9858EB6E2176B431FBd10d1eC --apply
   ```

   With Robinhood's configured CREATE3 factory and testnet salt, ASG is
   `0x87b9A22fC2d6b6dEaB3FfacCe3b5c6D252CEF9B8`. Gateway and ITS ownership
   remain unchanged at this stage.
3. Before handing over ownership, test governance delivery and operator
   execution with ASG's own `operator()` getter (`0x570ca735`). This creates
   a real testnet governance proposal and ASG execution, but does not change
   gateway or ITS state. It requires a funded Cosmos proposer, validator
   votes, and native gas on the ASG operator account. Review and approve the exact
   title, description and payload displayed by the command:

   ```bash
   ./target/release/axe propose testnet robinhood --type operator \
     --mnemonic-env TESTNET_PROPOSER_MNEMONIC \
     --target 0x87b9A22fC2d6b6dEaB3FfacCe3b5c6D252CEF9B8 \
     --calldata 0x570ca735 --relay
   ```

4. After successful execution, set `ASG_PRIVATE_KEY` to the current owner
   key for `0x49845e5d9985d8dc941462293ed38EEfF18B0eAE`. Keep the ASG
   operator key set. Preview and then approve each ownership transfer:

   ```bash
   ./target/release/axe asg testnet robinhood handoff-asg \
     --from 0x49845e5d9985d8dc941462293ed38EEfF18B0eAE \
     --rpc-url https://robinhood-testnet.drpc.org

   ./target/release/axe asg testnet robinhood handoff-asg \
     --from 0x49845e5d9985d8dc941462293ed38EEfF18B0eAE \
     --rpc-url https://robinhood-testnet.drpc.org --apply
   ```

5. Rerun the handoff preview to verify both owners are ASG and the operational
   roles are retained. Follow with the normal GMP/ITS smoke tests before
   treating the rehearsal as complete.

Verified rehearsal results on 2026-11-02:

- ASG deployment: `0x673921a8620007066de1d6c6a0acc70e22c624844798bd2dfa45dea68c181045`.
- Axelar testnet proposal `673` passed. Gateway proof submission:
  `0xfbb91fc0092d33af482733845cd8374fe950b83f4f1177326c754c51ec64123d`.
- ASG consumed the governance message and emitted `OperatorProposalApproved`:
  `0x6e8d62a79324f4e3c76bede641d7afcb29639a657d18274b4faf82e29915a69f`.
- Operator execution emitted `OperatorProposalExecuted` for ASG's own
  `operator()` call, with zero native value:
  `0x86078f8f7ce9b6513265606f7b488a50110486b4d7bed0c46e387b006b9c3fb9`.
- Gateway reports the message executed and ASG reports no remaining approval.
  Both ownership transfers simulate successfully from the current owner.
  No gateway/ITS ownership transfer has been sent as part of these checks.

The operator path is verified end to end. Timelock execution and post-handoff
GMP/ITS tests remain separate checks before declaring the full rehearsal complete.

The public Robinhood RPC intermittently returns `historical state ... is
not available` for finalized-state queries. These reads are retried up to three
times with visible backoff. Runtime mismatches and contract reverts are not
retried. If the requested state remains unavailable, the command stops while
retaining the required verification depth.

Set `ASG_RPC_URL` to a provider that serves Robinhood's finalized state, or pass
`--rpc-url <url>` after `deploy-asg` / `handoff-asg`. An explicit flag takes
precedence over the environment variable, which takes precedence over the
config's `rpc`. The override is used for reads, simulation and sending, and
still must match the configured chain ID. It is never written to the deployment
config. Keep credential-bearing URLs in a secure environment file, not command
arguments or shared config. Retry without `--apply` to check the provider first.
The CLI loads the existing `.env` with precedence over shell exports, so
update any conflicting signer entries there when switching roles.
For `propose`, use `--mnemonic-env TESTNET_PROPOSER_MNEMONIC` to select a separate
funded Cosmos testnet wallet while keeping the existing `MNEMONIC`. The selected
variable is used for both proposal submission and relay proof fees. Keep the
same flag when resuming the proposal.

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
