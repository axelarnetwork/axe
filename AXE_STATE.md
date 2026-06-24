# AXE — State of cross-chain test transfers

_Last updated: 2026-06-24. Branch: `feat/add-legacy-support`._

`axe test load-test` drives real cross-chain transfers (GMP `callContract` and
ITS `interchainTransfer`) and verifies them **on-chain**. A route is "✅
executed" only when the destination message reached `executed`
(`isCommandExecuted` / dest-app execution / `MessageExecuted`), not merely
`approved`.

This document is the validated state of what works, what is still open, and the
**known limitations** we need to tackle. It is grounded in three sources of
truth, in priority order:

1. The dispatch match in `src/commands/load_test/mod.rs` — defines the
   **supported route surface** (which `(protocol, test-type)` pairs run vs. bail).
2. The wired chain set in `.github/actions/run-loadtest/action.yml` `CHAIN_MAP`
   (23 chains) — the chains the harness can target.
3. The on-chain results in `axe-load-test-logs/*.json` — the **validated** truth.

Companion docs: per-pair dispatcher matrices in
[`docs/routes.md`](docs/routes.md) and chain-type coverage in
[`docs/load-test-coverage.md`](docs/load-test-coverage.md). ⚠️ Both are
currently **stale** (they still list the purged Flow/Fantom and omit 11 wired
chains) — reconciliation is tracked as a child task (see §7).

---

## 1. The one fact that frames everything

**On mainnet, every EVM chain is a legacy/consensus chain** — none has an
Amplifier `VotingVerifier`. So on mainnet:

- **EVM ↔ EVM = legacy ↔ legacy** (Amplifier-EVM does not exist on mainnet).
- **Amplifier** on mainnet means only the **non-EVM** chains: Solana, Sui,
  Stellar, XRPL.

Amplifier-EVM chains (with a `VotingVerifier`) exist only on **testnet**
(e.g. `monad-3`, `celo-sepolia`, `xrpl-evm`, `hyperliquid`). This is why
legacy-chain support is load-bearing for mainnet.

---

## 2. Supported route surface (from the dispatcher)

The 23 wired chains (`CHAIN_MAP`): Arbitrum, Avalanche, Base, Binance, Blast,
Ethereum, Filecoin, Hedera, Hyperliquid, Immutable, Kava, Linea, Mantle, Monad,
Moonbeam, Optimism, Polygon, Scroll, Solana, Stellar, Sui, XRPL, XRPL EVM.
Purged (gone from the repo): Flow, Fantom, Berachain, Plume, Centrifuge.

Chain-type level, what the dispatcher runs (✅) vs. bails on (see §6 for why):

### GMP (`callContract`)

| src ↓ \ dst → | EVM | Solana | Stellar | Sui | XRPL |
|---|---|---|---|---|---|
| **EVM**     | ✅ | ✅ | ✅ | ✅ | ✖ by design |
| **Solana**  | ✅ | ✅ | ✅ | ✅ | ✖ by design |
| **Stellar** | ✅ | ✅ | — | ✅ | ✖ by design |
| **Sui**     | ✅ | ✅ | ⛔ not built | — | ✖ by design |
| **XRPL**    | ✖ by design | ✖ | ✖ | ✖ | — |

### ITS (`interchainTransfer` via the hub)

| src ↓ \ dst → | EVM | Solana | Stellar | Sui | XRPL |
|---|---|---|---|---|---|
| **EVM**     | ✅ | ✅ | ✅ | ✅ | ✅ canonical XRP |
| **Solana**  | ✅ | ⛔ not built | ⛔ not built | ✅ | ⛔ not built |
| **Stellar** | ✅ | ⚠️ untrusted (§6.2) | — | ✅ | ⛔ not built |
| **Sui**     | ✅ | ✅ | ⛔ not built | — | ⛔ not built |
| **XRPL**    | ✅ canonical XRP | ⛔ not built | ⛔ not built | ⛔ not built | — |

`its-with-data` (transfer + dest contract call) is wired for `evm → sol` only.

GMP is the cross-chain **delivery** primitive; ITS rides the identical
verify→route→approve→execute path and additionally needs its token registered on
each endpoint. Validating GMP validates the delivery path for both.

---

## 3. Validated on-chain — executed end-to-end

Latest log per `(src, dst, protocol)`; a route appears here only when the
destination message reached `executed`. Chain ids carry the network
(`*-sepolia`/`*-2`/`*-3`/`*-q1` = testnet).

### 3a. Mainnet — legacy EVM ↔ legacy EVM (GMP)

avalanche → {base, binance, blast, ethereum, kava, moonbeam, polygon, scroll};
binance → {avalanche, fraxtal, immutable, linea}; immutable → binance;
kava → {avalanche, filecoin, moonbeam}; moonbeam → kava.

Chains exercised as **source**: avalanche, binance, immutable, kava, moonbeam.
Chains validated as **destination** (executed): avalanche, base, binance, blast,
ethereum, filecoin, fraxtal, immutable, kava, linea, moonbeam, polygon, scroll.
`kava → filecoin` confirms filecoin **executes on mainnet** (its testnet relayer
does not).

### 3b. Mainnet — non-EVM amplifier + legacy↔non-EVM (GMP & ITS)

| Route | Class | Proto |
|---|---|---|
| avalanche → solana | legacy-EVM → amplifier non-EVM | GMP ✅ |
| avalanche → stellar | legacy-EVM → amplifier non-EVM | GMP ✅ |
| avalanche → sui | legacy-EVM → amplifier non-EVM | GMP ✅ |
| stellar → arbitrum | amplifier non-EVM → legacy-EVM | GMP ✅ |
| stellar → solana | amplifier non-EVM ↔ non-EVM | GMP ✅ |
| solana → sui | amplifier non-EVM ↔ non-EVM | GMP ✅ + ITS ✅ |
| sui → {avalanche, hyperliquid} | non-EVM → EVM | GMP ✅ |
| sui → solana | non-EVM ↔ non-EVM | ITS ✅ |
| hedera → sui | EVM → non-EVM | GMP ✅ |
| hyperliquid → {hedera, stellar} | EVM → mixed | GMP ✅ |
| hyperliquid → stellar | EVM → non-EVM | ITS ✅ |
| xrpl ↔ xrpl-evm | XRPL canonical XRP | ITS ✅ both ways |
| xrpl-evm → {avalanche, solana} | amplifier-EVM → mixed | ITS ✅ |

### 3c. Testnet — amplifier-EVM pipeline (full voted→routed→approved→executed)

| Route | Class | Proto |
|---|---|---|
| avalanche → celo-sepolia | legacy-EVM → amplifier-EVM | GMP ✅ |
| avalanche → ethereum-sepolia | legacy-EVM → amplifier-EVM | GMP ✅ + ITS ✅ |
| avalanche → {mantle-sepolia, polygon-sepolia} | legacy → amplifier-EVM | GMP ✅ |
| avalanche → stellar-2026-q1-2 | legacy-EVM → amplifier non-EVM | ITS ✅ |
| hyperliquid → celo-sepolia | amplifier-EVM ↔ amplifier-EVM | GMP ✅ |
| hyperliquid → stellar-2026-q1-2 | amplifier-EVM → non-EVM | GMP ✅ + ITS ✅ |
| xrpl-evm → scroll | amplifier-EVM → legacy-EVM | GMP ✅ |
| xrpl-evm → ethereum-sepolia | amplifier-EVM ↔ amplifier-EVM | ITS ✅ |
| solana → stellar-2026-q1-2 | non-EVM → non-EVM | GMP ✅ |
| stellar-2026-q1-2 → {solana, hyperliquid} | non-EVM → mixed | GMP/ITS ✅ |
| monad-3 → {avalanche, ethereum-sepolia, hedera} | amplifier-EVM → mixed | GMP/ITS ✅ |
| linea-sepolia → immutable | amplifier-EVM → EVM | GMP ✅ |

---

## 4. Open — timed-out routes under triage (NOT limitations)

**A timeout is not a known limitation.** Per the project rule: when a run reports
`… : timed out`, the message may still have **executed on-chain** after the
verifier stopped polling. Each of these must be triaged on-chain (Axelarscan /
GMP-API):

- **If it executed on-chain** → this is a **bug**: either the verifier's
  inactivity buffer is too tight or its discovery logic is wrong. Fix it (raise
  the buffer / fix discovery), do **not** record it as a limitation.
- **If it did not execute** → reclassify it as a real limitation (gas
  underpayment, untrusted chain, broken relayer) in §6.

The verifier uses a **global inactivity timeout** (`INACTIVITY_TIMEOUT = 1000s`
in `verify/mod.rs`): if no tx advances any phase for 1000s, every remaining tx is
marked `{phase}: timed out`. So a route that executes *late* (slow last leg) is
wrongly failed — that is the prime bug-class to find here.

### Open routes (latest log), grouped by where they stalled

| Route | Proto | Stalled at | First hypothesis (must verify on-chain) |
|---|---|---|---|
| base → polygon | gmp | legacy approval | cheap/dear gas mismatch? |
| celo → optimism | gmp | legacy approval | CELO → ETH-gas underpay? |
| filecoin → immutable | gmp | legacy approval | ? |
| fraxtal → mantle | gmp | legacy approval | Mantle dest needs high gas? |
| mantle → kava | gmp | legacy approval | ? |
| mantle → linea | gmp | legacy approval | MNT → ETH-gas underpay? |
| polygon → blast | gmp | legacy approval | POL → ETH-gas underpay? |
| polygon → moonbeam | gmp | legacy approval | ? |
| celo → solana | gmp | cosmos routing | CELO gas underpay for Solana leg? |
| sui → solana | gmp | Solana execution | executed late? |
| stellar → hyperliquid | its | EVM approval | executed late? (doc previously claimed ✅) |
| solana → hedera | its | EVM execution | executed late? |
| solana → hyperliquid | its | EVM execution | executed late? |
| hyperliquid → solana | its | second-leg discovery | hub→dest leg not discovered |
| sui → hyperliquid | its | second-leg discovery | hub→dest leg not discovered |
| avalanche → monad-3 | gmp+its | EVM approval | monad-3 testnet executor flaky |
| hyperliquid → monad-3 | its | second-leg discovery | monad-3 second leg |
| hedera → monad-3 | its | (pending) | re-run |
| arbitrum-sepolia → filecoin-2 | gmp | legacy approval | filecoin-2 testnet relayer? |
| arbitrum-sepolia → moonbeam | gmp | legacy approval | ? |
| base-sepolia → filecoin-2 | gmp | legacy approval | filecoin-2 testnet relayer? |
| base-sepolia → {moonbeam, scroll} | gmp | legacy approval | ? |
| celo-sepolia → mantle-sepolia | gmp | VotingVerifier | votes not completing |
| monad-3 → polygon-sepolia | gmp | VotingVerifier | votes not completing |
| stellar-2026-q1-2 → avalanche | its | second-leg discovery | hub→dest leg not discovered |

The "second-leg discovery" cluster (ITS to/from hyperliquid, monad-3, avalanche)
and the "EVM execution" cluster (solana → hyperliquid/hedera) are the strongest
candidates for "executed-late" buffer/discovery bugs and should be triaged first.

---

## 5. Code fixes already on this branch

1. **Windowed `getLogs`** (`verify/legacy.rs`) — scans the dest gateway in
   100-block windows newest→oldest. Unblocks RPCs that cap the block range
   (polygon Amoy ~128 blocks).
2. **Legacy-gas detection** (`load_test/gas_mode.rs`, `EvmFeeMode`) — chains with
   no EIP-1559 (`baseFeePerGas == null`, e.g. Kava) get type-0 txs with explicit
   `gas_price`. Block is read as raw JSON so non-standard blocks (Moonbeam omits
   `mixHash`) don't break detection.
3. **Paris-bytecode fallback** (`helpers.rs`) — pre-Shanghai chains reject the
   default contract's `PUSH0`; axe probes with `eth_call` and deploys a
   PUSH0-free paris build only when needed.
4. **Single-tx main-wallet send** — `num_txs=1` sends from the main wallet (no
   subwallet funding / parked refund), unblocking thin-balance L2 sources.

---

## 6. Known limitations (the things to tackle)

These are real, characterized limits — design/protocol/trust/upstream — not
timeouts. Each is something we either accept or drive a fix for.

1. **XRPL has no executable layer (by design).** GMP is impossible in any
   direction touching XRPL. ITS is wired only against EVM endpoints via the
   canonical XRP wrapper (`xrpl ↔ xrpl-evm` validated). No XRPL→XRPL.
2. **ITS trusted-chain gaps.** `stellar → solana` ITS reverts with
   `Contract Error #7 (UntrustedChain)` — the Stellar testnet ITS contract has
   not added `solana` to its trusted-chains list. **Fix is upstream**: the
   contract owner runs `stellar/its.js add-trusted-chains solana` from
   `axelar-contract-deployments`. No axe code change needed. Other endpoint
   pairs may have analogous untrusted-chain gaps — surfaced as a clear revert,
   not a timeout.
3. **Unimplemented dispatcher arms** (bail with an explanatory message, by
   design until built): ITS `sol → stellar`, ITS `sol ↔ sol`, ITS `sui →
   stellar`, ITS `sui → xrpl`, GMP `sui → stellar`, GMP `sui → xrpl`,
   `xrpl → sui` (both protocols). `its-with-data` is `evm → sol` only.
4. **Sui-source GMP voter coverage (upstream).** `Example::gmp::send_call`
   messages from Sui's `GmpChannelId` are rarely voted by the testnet verifier
   set (~130-day p99 historically). The Sui source side is code-complete and
   lands on Axelar, but does not progress past "voted" until verifier coverage
   improves. ITS messages from Sui (`InterchainTokenService.ChannelId`) complete
   in ~20s and are unaffected.
5. **Optimism as a *source* on a fresh deploy** fails with "intrinsic gas too
   high" (an op-stack initcode quirk, every RPC). Works as a **destination** and
   as a source with a cached `SenderReceiver`. Pre-existing, unrelated to the
   legacy work.
6. **Hedera ITS deploy blocker (upstream).** Hedera is excluded from some
   amplifier route cycles pending an upstream Hedera ITS deploy fix (see the
   `TODO(hedera)` in `scripts/test_amplifier_routes.sh`). Hedera works as a
   GMP/ITS endpoint where the token is already deployed (e.g. `monad-3 → hedera`
   ITS ✅, `hedera → sui` GMP ✅).
7. **ITS requires the token registered on each endpoint** (a prerequisite, not a
   failure). The runner fails fast before transfer if a provided/cached token id
   is not registered on the destination.

---

## 7. Required GitHub secrets (private RPCs)

Naming: `<CHAIN_DISPLAY_UPPER>_<NETWORK_UPPER>_RPC` (e.g. `KAVA_MAINNET_RPC`).
Values come from the `axelarnetwork/infrastructure` upstreams (they carry API
keys, so they live only in secrets):

- **`BINANCE_*_RPC`** — required; public BNB RPCs block `getLogs`.
- `MOONBEAM_*_RPC`, `KAVA_*_RPC`, `FILECOIN_*_RPC`, `BLAST_*_RPC` — recommended
  (config defaults are flaky / rate-limited).

A run-time `--source-rpc` / `--destination-rpc` always overrides the secret.

---

## 8. Parallelized e2e validation

The route fleet is already scripted and CI-wired:

- `scripts/test_amplifier_routes.sh` — runnable route fleet (cycle-based so each
  chain is exercised as both source and destination).
- `.github/workflows/test-amplifier-routes.yml`,
  `cron-amplifier-{mainnet,testnet}.yml` — matrix-style parallel CI jobs.

Triage of the §4 open routes is parallelized across child tasks of
[MOU-2](/MOU/issues/MOU-2): QA re-runs each open route and checks on-chain final
state; Protocol Engineer fixes the buffer/discovery bugs that triage confirms and
reconciles the stale `docs/routes.md` / `docs/load-test-coverage.md` with the
23-chain set. AXE_STATE.md §3/§4 are updated as cells resolve.
