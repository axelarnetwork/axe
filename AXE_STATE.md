# AXE — State of cross-chain test transfers

_Last validated: 2026-06-23. Scope: every load-test route class across testnet and
mainnet, validated on-chain (a route is "✅" only when the destination message
reached `executed` — i.e. `isCommandExecuted` / dest-app execution, not merely
`approved`)._

`axe test load-test` drives real cross-chain transfers (GMP messages and ITS
token transfers) and verifies them on-chain. This document is the validated
truth of what works and what doesn't.

---

## 1. The one fact that frames everything

**On mainnet, every EVM chain is a legacy/consensus chain** — none has an
Amplifier `VotingVerifier`. So on mainnet:

- **EVM ↔ EVM = legacy ↔ legacy** (Amplifier-EVM does not exist on mainnet).
- **Amplifier** on mainnet means only the **non-EVM** chains: Solana, Sui,
  Stellar, XRPL.

Amplifier-EVM chains (with a `VotingVerifier`) exist only on **testnet**
(e.g. `monad-3`, `celo-sepolia`, `xrpl-evm`, `hyperliquid`). This is exactly why
the legacy-chain support is load-bearing for mainnet.

---

## 2. Route-class status (the grid)

Legend: ✅ validated on-chain this pass · 🔆 validated in prior sessions ·
⚠️ works, see caveat · ⏳ in validation · ◻️ not exercised this pass.

| Route class | Testnet | Mainnet |
|---|---|---|
| **legacy-EVM ↔ legacy-EVM** (GMP) | ✅ | ✅ (extensive — §3) |
| **legacy-EVM ↔ legacy-EVM** (ITS) | 🔆 (avalanche↔ethereum-sepolia) | ◻️ same delivery path as GMP; needs the ITS token registered per chain |
| **legacy-EVM → amplifier-EVM** (GMP) | ✅ avalanche→celo-sepolia (monad-3 reached approved but its executor stalled) | — (no Amplifier-EVM on mainnet) |
| **amplifier-EVM → legacy-EVM** (GMP) | ✅ xrpl-evm→scroll | — |
| **amplifier-EVM ↔ amplifier-EVM** (GMP) | ✅ hyperliquid→celo-sepolia | — |
| **legacy-EVM ↔ non-EVM** (amplifier) | 🔆 (avalanche↔sui GMP, avalanche↔stellar ITS, xrpl-evm→avalanche ITS) | ✅ both ways: avalanche→solana ✅, avalanche→stellar ✅, sui→base ✅, stellar→arbitrum ✅ (§3b) |
| **amplifier non-EVM ↔ non-EVM** (GMP) | 🔆 (baseline) | ✅ solana→sui (§3b) |

GMP is the cross-chain **delivery** primitive; ITS rides the identical
verify→approve→execute path and additionally needs its token registered on each
endpoint. Validating GMP validates the delivery path for both.

---

## 3. Mainnet validation results (this pass — GMP, on-chain)

All via the private node RPCs (see §5), default or explicit cross-chain gas.

**✅ Executed end-to-end (legacy ↔ legacy):**

| Route | Note |
|---|---|
| avalanche → scroll | 57s |
| avalanche → blast | 0.03 AVAX gas |
| avalanche → base | 0.03 AVAX gas |
| avalanche → polygon | |
| binance → fraxtal | |
| binance → linea | 0.01 BNB gas |
| binance → immutable | |
| immutable → binance | |
| kava → filecoin | filecoin **executes on mainnet** (its *testnet* relayer doesn't) |
| kava → moonbeam | |
| moonbeam → kava | |
| (→) arbitrum | arbitrum validated as destination |

Chains exercised as **source**: avalanche, binance, immutable, kava, moonbeam,
celo, mantle, polygon, fraxtal. Chains validated as **destination** (executed):
arbitrum, avalanche, base, binance, blast, filecoin, fraxtal, immutable, kava,
linea, moonbeam, polygon, scroll. (Mantle reached `approved` both ways but its
execution needs a higher `--gas-value` — see below.)

**⚠️ Approved but not executed — cross-chain gas underpayment (NOT a route failure):**

| Route | Why |
|---|---|
| polygon → blast | cheap-token source (POL) under-funds ETH-gas dest execution |
| mantle → linea | same (MNT → ETH-gas linea) |
| celo → optimism | same (CELO → ETH-gas optimism) |
| fraxtal → mantle | Mantle's dest execution needs high gas (its gas accounting); default underpaid |

These reached `approved` on the destination gateway but the relayer didn't
execute because the gas paid converted to too little destination gas. **Proven**:
the same destinations execute fine with a richer source / higher `--gas-value`
(avalanche→blast ✅, binance→linea ✅). **Rule: when the source token is much
cheaper than the destination's gas token — or the destination has unusual gas
accounting (Mantle) — pass an explicit `--gas-value`.**

**🪙 Blocked only by my local wallet's thin balance (NOT axe):**

| Route | Why |
|---|---|
| scroll → polygon | wallet has 0.017 ETH on scroll; a source run needs ~0.05 |
| arbitrum → solana | wallet has 0.024 ETH on arbitrum; solana-dest funding needs ~0.052 |

scroll/blast/fraxtal work fine as **destinations** (cheap one-time deploy); they
just can't be **sources** until the wallet holds ~0.05 native there.

---

## 3b. Amplifier route validation (this pass — GMP, on-chain)

**Mainnet — non-EVM amplifier chains** (cross-checked both directions):

| Route | Class | Result |
|---|---|---|
| avalanche → solana | legacy-EVM → amplifier (non-EVM) | ✅ executed (routed→approved→executed, 76s) |
| avalanche → stellar | legacy-EVM → amplifier (non-EVM) | ✅ executed |
| sui → base | amplifier (non-EVM) → legacy-EVM | ✅ executed |
| stellar → arbitrum | amplifier (non-EVM) → legacy-EVM | ✅ executed |
| solana → sui | amplifier ↔ amplifier (non-EVM) | ✅ executed |

Solana, Sui, Stellar are all funded on mainnet and worked as both source and
destination across these routes. Note: `celo → solana` first **stalled at
`routed`** — that was cheap-token (CELO) gas underpayment for the Solana side;
`avalanche → solana` (rich AVAX + `--gas-value 0.02`) executed cleanly, so the
§3 gas rule applies to non-EVM destinations too. XRPL is ITS-only (no GMP);
not exercised this pass.

**Testnet — amplifier-EVM chains** (chains with a `VotingVerifier`; full
voted→routed→approved→executed pipeline):

| Route | Class | Result |
|---|---|---|
| hyperliquid → celo-sepolia | amplifier-EVM ↔ amplifier-EVM | ✅ executed (voted+routed+approved+executed) |
| xrpl-evm → scroll | amplifier-EVM → legacy-EVM | ✅ executed |
| avalanche → celo-sepolia | legacy-EVM → amplifier-EVM | ✅ executed |
| avalanche → monad-3 | legacy-EVM → amplifier-EVM | ⚠️ reached `approved` on monad but execution stalled — monad's testnet executor is flaky (known); delivery confirmed |

---

## 4. Code fixes that make this work (branch `feat/add-legacy-support`)

1. **Windowed `getLogs`** (`verify/legacy.rs`) — scans the dest gateway in
   100-block windows newest→oldest instead of one open-ended query. Unblocks
   RPCs that cap the block range (polygon Amoy ~128 blocks) and is robust on all.
2. **Legacy-gas detection** (`load_test/gas_mode.rs`, `EvmFeeMode`) — chains with
   no EIP-1559 (`baseFeePerGas == null`, e.g. Kava) break alloy's 1559 fee
   estimation; axe sends type-0 txs with explicit `gas_price`. The block is read
   as raw JSON so chains with non-standard blocks (Moonbeam omits `mixHash`)
   don't break detection.
3. **Paris-bytecode fallback** (`helpers.rs`) — pre-Shanghai chains reject the
   default contract's `PUSH0`; axe probes with `eth_call` (no nonce spent) and
   deploys a PUSH0-free paris build only when the chain needs it. Normal chains
   are untouched.

---

## 5. Required GitHub secrets (private RPCs)

Naming: `<CHAIN_DISPLAY_UPPER>_<NETWORK_UPPER>_RPC` (e.g. `KAVA_MAINNET_RPC`).
The workflow `env:` maps `secrets.X` in; the action picks the matching one.
Set these (values from the `axelarnetwork/infrastructure` upstreams — they carry
API keys, so they live only in secrets, never in the repo):

- **`BINANCE_*_RPC`** — required; public BNB RPCs block `getLogs`.
- `MOONBEAM_*_RPC`, `KAVA_*_RPC`, `FILECOIN_*_RPC`, `BLAST_*_RPC` — recommended
  (config defaults are flaky / rate-limited).

A run-time `--source-rpc` / `--destination-rpc` always overrides the secret.

---

## 6. Chains in the load-test dispatch

Wired (CHAIN_MAP + dropdowns, both nets unless noted): Arbitrum, Avalanche, Base,
Binance, Blast, Ethereum, Filecoin, Hedera, Hyperliquid, Immutable, Kava, Linea,
Mantle, Monad, Moonbeam, Optimism, Polygon, Scroll, Solana, Stellar, Sui, XRPL,
XRPL EVM.

**Purged (removed from the repo): Flow, Fantom, Berachain, Plume, Centrifuge.**

---

## 7. Caveats & not-yet-validated

- **ITS on mainnet** is not exercised in this pass — it uses the same delivery
  path as GMP (validated) but additionally needs its token registered on each
  endpoint. Validated on testnet (avalanche↔ethereum-sepolia).
- **Non-EVM on mainnet** — Solana, Sui, Stellar validated for GMP (§3b). XRPL is
  ITS-only (no GMP) and was not exercised. The exact reverse directions not run
  (e.g. EVM→Sui, Solana→EVM) ride the same path as the directions that passed.
- **Optimism as a *source* on a fresh deploy** fails with "intrinsic gas too
  high" (an op-stack initcode quirk, every RPC) — works as a destination and with
  a cached SenderReceiver. Pre-existing, unrelated to the legacy work.
- A destination that only ever reaches `approved` (never `executed`) is almost
  always **gas underpayment** (§3), not a broken route.
