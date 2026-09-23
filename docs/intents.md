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
