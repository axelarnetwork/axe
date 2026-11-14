# Intents (`axe intents`)

Axelar intents are filled by a solver over the public RFQ API, not by the
Amplifier pipeline. A user deposits on the source chain, the solver pays out on
the destination, and the protocol settles with the solver afterwards. That is
why nothing here needs a relayer, a verifier set, or a gateway approval — and
why a route only works when the solver holds inventory on the destination.

Assets are named `<CAIP-2 chain>/<token address>`, for example
`eip155:43113/0x5425890298aed601595a70ab815c96711a31bc65`. `axe intents
catalog` lists every pair the API supports.

## Reading

```bash
axe intents catalog --json
axe intents catalog --chain eip155:43113
axe intents inventory --json
axe intents status <quote-id>
axe intents quote --from <asset> --to <asset> --amount 1.5 --json
```

`catalog` is the vocabulary: chains, tokens, decimals. `inventory` is what the
solver can actually fill, valued in USD — a route with no destination inventory
will not quote however well-funded the wallet is. `quote` asks for a price
without depositing; `--json` prints it and stops. `status` reads one quote's
state, and `--watch` polls it to a terminal state.

The twice-daily testnet workflow runs only `catalog` and `inventory`. Low solver
balances remain visible as warnings without failing the scheduled run. Quote
benchmarks and transfer checks require solver liquidity and remain available
through manual workflow runs (`smoke`, `canary`, `sweep`, or `full`). Catalog
and inventory command errors still fail the scheduled run.

## Spending

```bash
axe intents send --from <asset> --to <asset> --amount 1.5
axe intents roundtrip --from <asset> --to <asset>
axe intents sweep --sweeps 2
axe intents traffic
axe intents stress --symbol USDC --amount 0.1 --duration-secs 900
```

`send` deposits one intent. `roundtrip` sends one in each direction over the
same pair, so the wallet's balances end roughly where they started. `sweep`
does that across every route the wallet can currently execute. `traffic` keeps
doing it until interrupted. `stress` broadcasts concurrent deposits from every
funded source chain to find the deposit path's throughput ceiling, and is
testnet-only.

Every spending command takes a per-wallet lock, so two of them cannot use the
same wallet at once, whatever started them.

## Route checks (`axe test rfq`)

The RFQ API also quotes ITS and gateway transfers. `axe test rfq` checks those
routes end to end. For each route in turn it requests an `EXACT_INPUT` quote,
picks the first quote from the route's backend, and sends every quoted action
on the source chain in order. It then polls `/rfq/v1/status` for all the
transfers at once, every 15 seconds, until each one is `DONE`. `FAILED`,
`NOT_FOUND`, `REFUNDED`, a reverted action, or the timeout (45 minutes by
default) fail that route, and any failed route fails the command.
`REFUND_PENDING` and `REFUND_INITIATED` are reported and polling continues
until the refund lands or the timeout passes, so a refunded transfer always
fails. A failed status poll is retried.

```bash
axe --network testnet test rfq --route its,native-wrap,gateway,solana
axe --network testnet test rfq --route gateway --source sepolia
axe --network testnet test rfq --route solana --recipient <solana address>
axe --network testnet test rfq --route native-wrap --dry-run --sender <address>
```

| Route | Source token | Destination | Accepted backends |
|---|---|---|---|
| `its` | wrapped native (WAVAX, or WETH from Ethereum) | the same token on the other EVM chain | `its` |
| `native-wrap` | native AVAX or ETH, wrapped on the source | the wrapped token on the other EVM chain | `its` |
| `gateway` | gateway USDC (aUSDC on testnet) | the same token on the other EVM chain | `gateway`, `gateway-express` |
| `solana` | wrapped native | its Solana mint | `its` |

`--route` takes several routes, repeated or comma-separated. `--source` is
`avalanche` (Fuji on testnet, the default) or `ethereum` (Sepolia on testnet),
and `fuji` and `sepolia` are accepted too. The token addresses come from the
network configs rfq-api resolves quotes against, for testnet and mainnet.
Amounts are base units: 10^14 of the wrapped native token, or 1 USDC on the
gateway route. `--amount` overrides it for a single route.

The key comes from `EVM_PRIVATE_KEY`, then `PRIVATE_KEY`, or `--private-key`.
The recipient defaults to the sender, and on the `solana` route to the address
of the `SOLANA_PRIVATE_KEY` keypair file (else `~/.config/solana/id.json`).
`--recipient` replaces it on every route, so it must suit each route's
destination. When the wallet holds less of the wrapped native token than the
amount, the `its` and `solana` routes wrap the difference first with the
token's `deposit()`. The source RPC comes from the chains config, with the
intent RPC overrides applied. The wallet lock is held only while the actions
are sent.

`--dry-run` quotes, checks that every action is a transaction on the source
chain from the sender, and prints the actions without sending anything. It
needs only `--sender` or a key. `--json` prints a summary object on stdout
(per route: quote, backend, action transactions, last status, error) and moves
progress to stderr.

The `Test RFQ routes` workflow runs all four routes on testnet every 6 hours,
in one invocation and in the `intents-testnet` concurrency group because it
shares the intents wallet. Mainnet runs only through a manual dispatch, which
also picks the routes and the source chain. The step summary lists each route
with ✓ or ✗, and any failed route fails the run.

## Through MCP

The same flows are tools on the MCP server, with three differences that follow
from an agent rather than a person driving them:

- **The spending flows detach.** `intents_send`, `intents_roundtrip`,
  `intents_sweep`, `intents_traffic` and `intents_stress` return a run
  identifier immediately; `run_report` reads the result once it lands. A
  fulfillment can take longer than a client will hold a request open, and a
  cancelled request would lose the record of a deposit already made.
- **Every run is bounded up front.** `max_intents` is required on the flows
  that pick their own routes, and reserved against the operator's budget
  before anything is quoted. `intents_traffic` additionally requires a
  duration, because nothing else would stop it.
- **A chain allowlist narrows what they can find.** The operator's
  `--allow-chain` list is applied to the chains config the run loads, and
  every flow discovers its routes by resolving the RFQ catalog against that
  config. A chain outside the list therefore resolves to nothing, is never
  discovered, and never appears in a route — so the flows still run, on the
  chains that were allowed. A route you name yourself is refused by name
  instead. An allowlist that matches nothing the solver serves comes back as
  "no funded routes", which is the honest answer.

The read-only tools — `intents_catalog`, `intents_inventory`, `intents_quote`,
`intents_status`, `intents_bench_quote` — answer in the call, and spend
nothing.
