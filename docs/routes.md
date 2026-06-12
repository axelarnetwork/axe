# Supported routes

The matrices below show which protocols (`GMP`, `ITS`, or both) `axe test
load-test` dispatches for each (source → destination) chain pair, by network.

See [load-test-coverage.md](load-test-coverage.md) for the chain-type-level
dispatcher matrix, per-environment chain availability, and verified-on-testnet
status per pair.

## Mainnet

| source ↓ / destination → | Arbitrum | Avalanche | Base | Ethereum | Flow | Hyperliquid | Monad | Optimism | Solana | Stellar | Sui | XRPL | XRPL EVM |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Arbitrum    | —        | GMP+ITS  | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS     | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS   | GMP+ITS  | ITS  | GMP+ITS  |
| Avalanche   | GMP+ITS  | —        | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS     | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS   | GMP+ITS  | ITS  | GMP+ITS  |
| Base        | GMP+ITS  | GMP+ITS  | —       | GMP+ITS  | GMP+ITS  | GMP+ITS     | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS   | GMP+ITS  | ITS  | GMP+ITS  |
| Ethereum    | GMP+ITS  | GMP+ITS  | GMP+ITS | —        | GMP+ITS  | GMP+ITS     | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS   | GMP+ITS  | ITS  | GMP+ITS  |
| Flow        | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS  | —        | GMP+ITS     | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS   | GMP+ITS  | ITS  | GMP+ITS  |
| Hyperliquid | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS  | GMP+ITS  | —           | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS   | GMP+ITS  | ITS  | GMP+ITS  |
| Monad       | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS     | —       | GMP+ITS  | GMP+ITS  | GMP+ITS   | GMP+ITS  | ITS  | GMP+ITS  |
| Optimism    | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS     | GMP+ITS | —        | GMP+ITS  | GMP+ITS   | GMP+ITS  | ITS  | GMP+ITS  |
| Solana      | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS     | GMP+ITS | GMP+ITS  | —        | GMP+ITS\* | GMP+ITS\*| —    | GMP+ITS  |
| Stellar     | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS     | GMP+ITS | GMP+ITS  | GMP+ITS\*| —         | GMP+ITS\*| —    | GMP+ITS  |
| Sui         | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS     | GMP+ITS | GMP+ITS  | GMP+ITS\*| GMP+ITS\* | —        | —    | GMP+ITS  |
| XRPL        | ITS      | ITS      | ITS     | ITS      | ITS      | ITS         | ITS     | ITS      | —        | —         | —        | —    | ITS      |
| XRPL EVM    | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS     | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS   | GMP+ITS  | ITS  | —        |

## Testnet

| source ↓ / destination → | Arbitrum | Avalanche | Base | Ethereum | Flow | Hedera | Hyperliquid | Monad | Optimism | Solana | Stellar | Sui | XRPL | XRPL EVM |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Arbitrum    | —        | GMP+ITS  | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS     | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS   | GMP+ITS  | ITS  | GMP+ITS  |
| Avalanche   | GMP+ITS  | —        | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS     | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS   | GMP+ITS  | ITS  | GMP+ITS  |
| Base        | GMP+ITS  | GMP+ITS  | —       | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS     | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS   | GMP+ITS  | ITS  | GMP+ITS  |
| Ethereum    | GMP+ITS  | GMP+ITS  | GMP+ITS | —        | GMP+ITS  | GMP+ITS | GMP+ITS     | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS   | GMP+ITS  | ITS  | GMP+ITS  |
| Flow        | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS  | —        | GMP+ITS | GMP+ITS     | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS   | GMP+ITS  | ITS  | GMP+ITS  |
| Hedera      | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS  | GMP+ITS  | —       | GMP+ITS     | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS   | GMP+ITS  | ITS  | GMP+ITS  |
| Hyperliquid | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS | —           | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS   | GMP+ITS  | ITS  | GMP+ITS  |
| Monad       | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS     | —       | GMP+ITS  | GMP+ITS  | GMP+ITS   | GMP+ITS  | ITS  | GMP+ITS  |
| Optimism    | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS     | GMP+ITS | —        | GMP+ITS  | GMP+ITS   | GMP+ITS  | ITS  | GMP+ITS  |
| Solana      | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS     | GMP+ITS | GMP+ITS  | —        | GMP+ITS\* | GMP+ITS\*| —    | GMP+ITS  |
| Stellar     | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS     | GMP+ITS | GMP+ITS  | GMP+ITS\*| —         | GMP+ITS\*| —    | GMP+ITS  |
| Sui         | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS     | GMP+ITS | GMP+ITS  | GMP+ITS\*| GMP+ITS\* | —        | —    | GMP+ITS  |
| XRPL        | ITS      | ITS      | ITS     | ITS      | ITS      | ITS     | ITS         | ITS     | ITS      | —        | —         | —        | —    | ITS      |
| XRPL EVM    | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS | GMP+ITS     | GMP+ITS | GMP+ITS  | GMP+ITS  | GMP+ITS   | GMP+ITS  | ITS  | —        |

### Asymmetric pairs (the `*`)

The matrix shows the union of what's available in either direction; an asterisk
means the forward and reverse legs differ.

- **Solana ↔ Stellar** — `Solana → Stellar` dispatches GMP only (ITS is not wired in axe for Sol→Stellar). `Stellar → Solana` dispatches GMP+ITS.
- **Solana ↔ Sui** — `Solana → Sui` dispatches GMP+ITS. The reverse (`Sui → Solana`) is unwired in axe today.
- **Stellar ↔ Sui** — `Stellar → Sui` dispatches GMP+ITS. The reverse (`Sui → Stellar`) is unwired in axe today.

### Notes

- `XRPL` has no executable layer, so GMP is unavailable for any pair touching it; ITS is only wired against EVM destinations (via the canonical XRP wrapper).
- A cell saying `GMP+ITS` means axe's *dispatcher* supports the route. Live success additionally depends on per-chain ITS trusted-chains and token registration — see `scripts/test_amplifier_routes.sh` for the runnable fleet.
- Self-pairs (e.g. `Sui → Sui`) are not load-test targets; the only same-chain-type route axe dispatches is `Solana → Solana` GMP.
