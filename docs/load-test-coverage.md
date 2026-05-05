# Load-test coverage matrix

What `axe test load-test` supports per (source, destination, protocol) combination, and on which Axelar environments. Pairs not listed are unsupported by Axelar today (e.g. XRPL is ITS-only because it has no smart contracts).

## GMP (`callContract`)

| Source ↓ / Dest → | EVM | Solana | Stellar | Sui | XRPL |
|---|---|---|---|---|---|
| **EVM** | ✅ | ✅ | ✅ | ✅ ² | n/a |
| **Solana** | ✅ | ✅ | ✅ | ✅ ² | n/a |
| **Stellar** | ✅ | ✅ | n/a | ✅ ² | n/a |
| **Sui** | ⚠️ ¹ | (not built yet) | (not built yet) | — | n/a |
| **XRPL** | n/a | n/a | n/a | n/a | n/a |

¹ `axe test load-test --source-chain sui --destination-chain <evm> --protocol gmp` is wired and the source-side Sui transaction lands correctly (verified via Sui RPC + Axelarscan). However, the Axelar voter set processing **`Example::gmp::send_call` messages** (sender = `Example.objects.GmpChannelId`) does not consistently vote on them — only one historical message from that channel has ever progressed past "called", and it took 130 days. **ITS messages from Sui** (sender = `InterchainTokenService.objects.ChannelId`) complete in ~20s. Sui GMP source side is code-complete but upstream-blocked until voter coverage improves.

² **GMP → Sui works end-to-end on testnet**. Verified runs:
- `xrpl-evm → sui` GMP: 73 s (voted 39 s, routed 5 s, approved 19 s, executed 11 s)
- `solana → sui`  GMP: 53 s (voted 25 s, routed 5 s, approved 6 s, executed 16 s)
- `stellar-2026-q1-2 → sui` GMP: 63 s (voted 15 s, routed 5 s, approved 19 s, executed 17 s)

The verifier polls Sui's `events::MessageApproved` and `events::MessageExecuted` on the `AxelarGateway` Move package via cursor-paginated `suix_queryEvents`. Source-side senders are unchanged; destination contract ID is read from `chains.sui.contracts.Example.objects.GmpChannelId`.

The remaining Sui variants (Sui-source ITS, all `*-to-sui` ITS, and `xrpl-to-sui`) are wired in the CLI/dispatch but bail with informative messages identifying what's needed to complete them. See "Outstanding Sui implementation work" below.

XRPL has no contract execution model — it can only carry token payments via ITS, never `callContract`.

## ITS (`interchainTransfer` via hub)

| Source ↓ / Dest → | EVM | Solana | Stellar | Sui | XRPL |
|---|---|---|---|---|---|
| **EVM** | (use evm-to-evm) | ✅ | ✅ | (not built yet) | ✅ canonical XRP |
| **Solana** | ✅ | ✅ | ❌ not implemented | (not built yet) | ❌ not implemented |
| **Stellar** | ✅ | ⚠️ ¹ | n/a | (not built yet) | ❌ not implemented |
| **Sui** | (not built yet) | (not built yet) | (not built yet) | — | (not built yet) |
| **XRPL** | ✅ canonical XRP | ❌ not implemented | ❌ not implemented | (not built yet) | n/a |

¹ Module `its_stellar_to_sol` exists and the dispatch is wired, but the **Stellar testnet ITS contract** (`CC7L…M5YP`) has not added `solana` to its trusted-chains list. The run reaches Stellar simulation and reverts with `Contract Error #7 (UntrustedChain)`. The fix is upstream: the contract owner runs `ts-node stellar/its.js add-trusted-chains solana` from `axelar-contract-deployments`. Once that's in, this pair starts working with no code changes here.

## Outstanding Sui implementation work

✅ **DONE — Sui destination + EVM/Sol/Stellar → Sui GMP** (`DestinationChecker::Sui`, `verify_onchain_sui_gmp[_streaming]`, `run_evm_to_sui`, `run_sol_to_sui`, `run_stellar_to_sui`). Verifier uses cursor-paginated `suix_queryEvents` on `events::MessageApproved` and `events::MessageExecuted` from the `AxelarGateway` Move package.

What's left:

1. **Sui-source ITS (`its_sui_to_evm`, `its_sui_to_sol`, `its_sui_to_stellar`, `its_sui_to_xrpl`)** —
   - Add `interchain_transfer` PTB construction to `src/sui.rs`. The Move signature is `interchain_token_service::interchain_transfer<T>(its, channel, gateway, gas_service, coin: Coin<T>, dest_chain: String, dest_address: String, payload: vector<u8>, gas_token: Coin<SUI>, refund_address: address)`.
   - The type parameter `T` (coin type) must be resolved from `--token-id` via `interchain_token_service::registered_coin_type(its, token_id)`, executed as a `sui_devInspectTransactionBlock` call (read-only). Returns the Move type tag string for `T`.
   - Once `T` is known, the runner picks an existing `Coin<T>` object owned by the sender, splits off the transfer amount, and constructs the PTB.
   - For "out-of-the-box" support like the EVM/Sol AXE deploy pattern, you'd also need to publish a Move package on Sui that mints a fresh canonical `AXE` coin and registers it via `interchain_token_service::register_coin<AXE>`. That's a Move-bytecode publish step not covered by the current Cargo build pipeline; the simplest path is to require `--token-id <hex>` for Sui-source ITS until a dedicated `axe deploy sui-its-token` subcommand is added.

2. **ITS to Sui (`evm/sol/stellar/xrpl → sui`)** — the destination Sui side needs the relayer to call `interchain_token_service::receive_interchain_transfer<T>`. Since that requires the registered coin type `T` per token_id, the runner has to look up the token's Move type via the same `registered_coin_type` dev-inspect as (1) and pre-create a Coin<T> escrow if needed. The destination verifier is already shipped (it reads the same `MessageExecuted` event the relayer's call emits via the gateway), so once the source-side runner addresses (1)'s coin handling, (2) follows by reusing the existing `verify_onchain_sui_gmp_streaming`.

3. **Sui-source GMP voter coverage** — `Example::gmp::send_call` messages from Sui's `GmpChannelId` are currently rarely voted on by the Axelar testnet voter set (~130 days p99 from history). Source side runs and lands on Axelar but doesn't progress to "voted". Mitigation lives upstream — once the verifier set picks these up, no axe changes needed.

Until (1)/(2) land, the bail messages from the dispatcher tell the user exactly what's missing, and the upstream `axelar-contract-deployments/sui/{gmp,its}.js` scripts remain the path of last resort for hand-driven Sui ITS flows.

## Per-environment chain availability

Whether a pair works also depends on whether both chains are deployed on the chosen environment:

| Env | EVM chains | Solana | Stellar | Sui | XRPL | XRPL-EVM | Notes |
|---|---|---|---|---|---|---|---|
| **testnet** | many | `solana` | `stellar-2026-q1-2` | `sui` | `xrpl` | `xrpl-evm` | most coverage |
| **stagenet** | many | `solana-stagenet-3` | none | `sui` | `xrpl` | `xrpl-evm` | no Stellar |
| **devnet-amplifier** | `avalanche-fuji`, others | `solana-18` | none | `sui-2` | `xrpl-dev` | `xrpl-evm-devnet` ⚠️ | no Stellar; xrpl-evm-devnet AxelarGateway/ITS not deployed at configured addresses (`eth_getCode` returns `0x`) — GMP/ITS to it falls through pre-flight bytecode check |
| **mainnet** | many | `solana` | `stellar` | `sui` | `xrpl` | `xrpl-evm` | full coverage; Solana program IDs are feature-gated and resolve to mainnet (`gtwqvLL…`, `gaszjG…`, `memtaCu…`, `itsAUdH…`) when built `--features mainnet --no-default-features` |

## Resolving how a `--protocol`/`--source-chain`/`--destination-chain` triple is dispatched

Auto-detect runs through these steps in order:
1. If both `--source-chain` and `--destination-chain` are provided, the chain types are read from the config (`chainType: evm | svm | stellar | xrpl`) and combined into a `TestType`.
2. Otherwise, the user-provided `--test-type` is honored.
3. The combined `(Protocol, TestType)` selects an `its_*` or `run_*` runner in [`src/commands/load_test/mod.rs`](../src/commands/load_test/mod.rs).

## Required env vars / flags by chain

| Chain | What's needed | Where |
|---|---|---|
| EVM (any) | `EVM_PRIVATE_KEY` (32-byte hex secp256k1) | `.env` or `--private-key` |
| Solana | `SOLANA_PRIVATE_KEY` pointing at a JSON keypair file (defaults to `~/.config/solana/id.json`) | `.env` or `--keypair` |
| Stellar | `STELLAR_PRIVATE_KEY` (`S…` secret or 32-byte hex) — used as both signer **and** receiver in stellar-to-* flows | `.env` |
| Sui | `SUI_PRIVATE_KEY` (`suiprivkey1…` bech32 from `sui keytool export` — supports both ed25519 (flag 0x00) and secp256k1 (flag 0x01); also accepts a 32-byte hex secret as ed25519). Get testnet SUI from https://faucet.sui.io | `.env` |
| XRPL (sender) | `XRPL_PRIVATE_KEY` (s-prefix family seed, e.g. `snr…` — falls back to `EVM_PRIVATE_KEY` bytes if unset) | `.env` |
| XRPL (receiver) | hardcoded per network in `src/commands/load_test/its_evm_to_xrpl.rs`. Mainnet receiver is the address derived from the operator's XRPL_PRIVATE_KEY; testnet/devnet/stagenet share a separate hardcoded address. Override is intentional, not via flag — change the const. |

## RPC overrides

| Override | Effect | Default |
|---|---|---|
| `--source-rpc` / `SOURCE_RPC` | source chain RPC URL | from chain config |
| `--destination-rpc` / `DESTINATION_RPC` | destination chain RPC URL | from chain config |
| `AXELAR_LCD_URL` | Axelar Cosmos REST endpoint | from chain config; auto-falls back to `lavenderfive` and `publicnode` on 5xx |
| `AXELAR_RPC_URL` | Axelar Tendermint RPC endpoint | from chain config; auto-falls back to `axelar-rpc.publicnode.com` and `rpc.cosmos.directory/axelar` on 5xx |

## All `axe test load-test` flags

Every flag (other than `--config`) is optional. Defaults are picked from the chain config + env feature flag.

| Flag | Type | Notes |
|---|---|---|
| `--config <path>` | path | **required**. Picks the env (`mainnet.json`, `testnet.json`, `stagenet.json`, `devnet-amplifier.json`). Binary's compiled feature must match. |
| `--source-chain <axelarId>` | string | Auto-detected when only one chain of the source type exists. Required for Sui pairs and any ambiguous cases. |
| `--destination-chain <axelarId>` | string | Same as source. |
| `--test-type <enum>` | one of `sol-to-evm \| evm-to-sol \| evm-to-evm \| sol-to-sol \| xrpl-to-evm \| evm-to-xrpl \| stellar-to-evm \| evm-to-stellar \| stellar-to-sol \| sol-to-stellar \| sui-to-evm \| evm-to-sui \| sol-to-sui \| sui-to-sol \| stellar-to-sui \| sui-to-stellar \| xrpl-to-sui \| sui-to-xrpl` | Auto-detected from the chain types — only set this if you want to override. |
| `--protocol <gmp \| its \| its-with-data>` | enum | Default `gmp`. `its-with-data` only supports `evm-to-sol`. |
| `--num-txs <N>` | u64 | Burst-mode tx count (default 5). |
| `--tps <N>` + `--duration-secs <N>` | u64 | Sustained-mode (EVM↔Solana only today). Pool size = `tps × key_cycle`. |
| `--key-cycle <N>` | u64 | Sustained-mode wallet rotation (default 3). Higher reduces per-address mempool pressure. |
| `--source-rpc <url>` / `--destination-rpc <url>` | string | Override the per-chain RPC URLs from config. Also via `SOURCE_RPC` / `DESTINATION_RPC` env. |
| `--private-key <hex>` | string | EVM key. Also via `EVM_PRIVATE_KEY` env. |
| `--keypair <path>` | path | Solana JSON keypair. Also via `SOLANA_PRIVATE_KEY` env. Defaults to `~/.config/solana/id.json`. |
| `--gas-value <wei/lamports/stroops/mist>` | string | Cross-chain gas attached to the source-side message. Default per source chain. |
| `--token-id <hex>` | string | Skip auto-token-deploy and use an existing ITS token id (e.g. canonical XRP `0xba5a21ca…`). |
| `--payload <hex>` | string | Override the auto-generated payload. |
| `--extra-accounts <N>` | u32 | Solana ITS-with-data only — extra accounts in the executable payload. |

## Solana commitment level

All Solana RPC clients in load-test paths (sender, verifier, keypairs) use `CommitmentConfig::finalized` since [src/solana.rs](../src/solana.rs) and the load-test modules. Earlier we used `confirmed` (faster ~5 s vs ~13–30 s) but this caused vote splits on mainnet: some Axelar verifiers query Solana at `confirmed`, others at `finalized`, so a tx confirmed-but-not-finalized could be voted on at mixed visibility — leading to `5Y / 5N` polls expiring as `Failed`. Finalized adds latency to the source confirm step but eliminates the race; net end-to-end time is roughly unchanged because the Axelar voter pass is faster when all queries see consistent state.

The `decode sol-tx` and `decode sol-activity` subcommands stay on `confirmed` — read-only diagnostic paths where the lower-latency commitment doesn't risk consistency.

## Building per environment

The binary feature-gates the Axelar amplifier program IDs at compile time. Pick exactly one of `mainnet | testnet | stagenet | devnet-amplifier`:

```sh
cargo build --release --no-default-features --features testnet
cargo build --release --no-default-features --features mainnet
cargo build --release --no-default-features --features stagenet
cargo build --release --no-default-features --features devnet-amplifier
```

If you point a binary built with one feature at a config from another env, the runner bails immediately:
```
Error: binary was compiled for 'devnet-amplifier' but config targets 'testnet'.
       Rebuild with: cargo build --release --features testnet --no-default-features
```
