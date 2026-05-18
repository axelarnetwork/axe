#!/usr/bin/env python3
"""
ITS trust-pair auditor for an Axelar network.

Walks every chain in the chains-config that has an InterchainTokenService
contract and queries its on-chain trust list. Output:

  * per-chain trust list,
  * asymmetric pairs (X trusts Y but Y doesn't trust X),
  * a "missing-trust" report for any chain (default: solana).

Supported chain types: EVM (anything with `isTrustedChain(string)` view) and
Solana (decodes the ITS root PDA's borsh-encoded `trusted_chains: Vec<String>`).

Sui, Stellar, and XRPL are skipped: Sui needs a `sui_devInspectTransactionBlock`
Move call we don't (yet) build here, Stellar's testnet contract address
rotates and its mainnet trust list lives behind a Soroban simulate, and XRPL
has no on-chain ITS smart contract (its trust is Hub-side).

The `stellar-2026-q1-2` testnet entry is skipped unconditionally on testnet
since its trust list resets with each test-config refresh.

Usage:
    NETWORK=testnet python3 scripts/audit_its_trust.py
    NETWORK=mainnet python3 scripts/audit_its_trust.py [--focus <chain>]

Requires:
    python3 -m pip install --break-system-packages solders base58
"""

import argparse
import base64
import json
import os
import struct
import sys
import urllib.request
import urllib.error
from pathlib import Path

import base58
from solders.pubkey import Pubkey


# ---------------------------------------------------------------------------
# CLI + config loading
# ---------------------------------------------------------------------------


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument(
        "--config",
        default=os.environ.get("CONFIG"),
        help="path to chains-config JSON (default: from $CONFIG or derived from $NETWORK)",
    )
    p.add_argument(
        "--network",
        default=os.environ.get("NETWORK"),
        help="network name (testnet | mainnet | stagenet | devnet-amplifier)",
    )
    p.add_argument(
        "--focus",
        default="solana",
        help="chain to single out in the 'missing trust' report (default: solana)",
    )
    return p.parse_args()


def load_config(args) -> tuple[dict, str]:
    if args.config:
        cfg_path = Path(args.config)
    elif args.network:
        cfg_path = Path(
            f"../axelar-contract-deployments/axelar-chains-config/info/{args.network}.json"
        )
    else:
        sys.exit("error: --network or --config required (or $NETWORK env var)")
    if not cfg_path.exists():
        sys.exit(f"error: chains config not found at {cfg_path}")
    with cfg_path.open() as f:
        return json.load(f), args.network or cfg_path.stem


# ---------------------------------------------------------------------------
# Chain classification
# ---------------------------------------------------------------------------


def classify_chain(chain_id: str, value: dict) -> str:
    """Return one of: evm, solana, sui, xrpl, stellar, unknown."""
    its = value.get("contracts", {}).get("InterchainTokenService", {})
    addr = its.get("address", "")
    if addr.startswith("0x"):
        return "evm"
    if chain_id == "solana" or chain_id.startswith("solana"):
        return "solana"
    if chain_id == "sui" or chain_id.startswith("sui"):
        return "sui"
    if chain_id.startswith("stellar"):
        return "stellar"
    if chain_id.startswith("xrpl") and not chain_id.startswith("xrpl-evm"):
        return "xrpl"
    return "unknown"


# ---------------------------------------------------------------------------
# JSON-RPC helpers
# ---------------------------------------------------------------------------


def json_rpc(url: str, method: str, params: list, timeout: float = 10.0):
    """One-shot JSON-RPC POST. Returns the `.result` field or raises on error."""
    payload = json.dumps({"jsonrpc": "2.0", "method": method, "params": params, "id": 1}).encode()
    req = urllib.request.Request(
        url,
        data=payload,
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        body = resp.read().decode()
    parsed = json.loads(body)
    if "error" in parsed:
        raise RuntimeError(f"rpc error: {parsed['error']}")
    return parsed.get("result")


# ---------------------------------------------------------------------------
# EVM ITS trust query
# ---------------------------------------------------------------------------


# keccak256("isTrustedChain(string)")[:4]
EVM_IS_TRUSTED_CHAIN_SELECTOR = "0xcaca8dbe"


def evm_is_trusted_calldata(chain_name: str) -> str:
    """ABI-encode `isTrustedChain(string chain)` calldata."""
    # offset to dynamic data (0x20 = 32 bytes from start of args)
    offset = "20".rjust(64, "0")
    length = format(len(chain_name), "064x")
    raw = chain_name.encode("utf-8")
    padded = raw.ljust(((len(raw) + 31) // 32) * 32, b"\x00").hex()
    return EVM_IS_TRUSTED_CHAIN_SELECTOR + offset + length + padded


def evm_query_trust(rpc: str, its: str, chain_name: str, retries: int = 1) -> bool | None:
    """Returns True/False or None on error (after retries)."""
    import time as _t

    data = evm_is_trusted_calldata(chain_name)
    for attempt in range(retries + 1):
        try:
            result = json_rpc(
                rpc,
                "eth_call",
                [{"to": its, "data": data}, "latest"],
                timeout=6.0,
            )
            if isinstance(result, str):
                return int(result, 16) != 0
            return None
        except (urllib.error.URLError, RuntimeError, ValueError):
            if attempt < retries:
                # Tiny backoff — public RPCs throttle aggressively; a longer
                # wait just inflates total runtime without rescuing anything.
                _t.sleep(0.3)
    return None


def evm_rpc_alive(rpc: str) -> bool:
    """Quick `eth_chainId` probe so we don't spend 30s of retries on a chain
    whose public RPC is dead — once we know the endpoint is unresponsive,
    we skip all the per-candidate queries and mark the row as unknown."""
    try:
        result = json_rpc(rpc, "eth_chainId", [], timeout=4.0)
        return isinstance(result, str) and result.startswith("0x")
    except (urllib.error.URLError, RuntimeError, ValueError):
        return False


# ---------------------------------------------------------------------------
# Solana ITS trust list
# ---------------------------------------------------------------------------


def solana_its_root_pda(program_id_b58: str) -> Pubkey:
    pid = Pubkey.from_string(program_id_b58)
    pda, _bump = Pubkey.find_program_address([b"interchain-token-service"], pid)
    return pda


def borsh_read_string(data: bytes, offset: int) -> tuple[str, int]:
    (length,) = struct.unpack_from("<I", data, offset)
    offset += 4
    s = data[offset : offset + length].decode("utf-8")
    return s, offset + length


def solana_fetch_trusted_chains(rpc: str, program_id_b58: str) -> list[str]:
    pda = solana_its_root_pda(program_id_b58)
    result = json_rpc(
        rpc,
        "getAccountInfo",
        [str(pda), {"encoding": "base64", "commitment": "finalized"}],
    )
    if result is None or result.get("value") is None:
        raise RuntimeError(
            f"ITS root PDA {pda} not found on this Solana RPC — wrong program id ({program_id_b58})?"
        )
    data_b64 = result["value"]["data"][0]
    data = base64.b64decode(data_b64)
    # 8-byte account discriminator at the head (Solana-ITS uses an Anchor-style
    # tag — the actual bytes are `a0e264d86bc9a543` on testnet but the value
    # is stable per account type, not per chain). Skip it.
    # After that, borsh layout:
    #   its_hub_address: String
    #   chain_name: String
    #   paused: bool
    #   trusted_chains: Vec<String>
    #   bump: u8
    offset = 8
    _hub, offset = borsh_read_string(data, offset)
    _chain_name, offset = borsh_read_string(data, offset)
    offset += 1  # paused (u8)
    (count,) = struct.unpack_from("<I", data, offset)
    offset += 4
    trusted: list[str] = []
    for _ in range(count):
        s, offset = borsh_read_string(data, offset)
        trusted.append(s)
    return trusted


# ---------------------------------------------------------------------------
# Audit driver
# ---------------------------------------------------------------------------


def chain_axelar_id(chain_id: str, value: dict) -> str:
    return value.get("axelarId") or chain_id


def rpc_override(chain_id: str, network: str) -> str | None:
    """Look for an explicit RPC override in the environment.

    Matches the GH composite action's naming so the same `.env` works here:
        `<CHAIN_UPPER>_<NETWORK_UPPER>_RPC` with `-` mapped to `_`.
    Example: `xrpl-evm` on `testnet` → `XRPL_EVM_TESTNET_RPC`.
    """
    key = f"{chain_id.upper().replace('-', '_')}_{network.upper().replace('-', '_')}_RPC"
    val = os.environ.get(key)
    return val if val else None


def gather_trust_lists(config: dict, network: str) -> dict[str, dict]:
    """Returns { chain_id: {"axelar_id": ..., "kind": ..., "trusted": [...] | None, "error": str|None } }."""
    out: dict[str, dict] = {}
    for chain_id, value in config["chains"].items():
        its = value.get("contracts", {}).get("InterchainTokenService", {})
        if not its.get("address"):
            continue
        if network == "testnet" and chain_id == "stellar-2026-q1-2":
            print(f"  - {chain_id}: skipped (stellar testnet rotates)", file=sys.stderr)
            continue
        kind = classify_chain(chain_id, value)
        rpc = rpc_override(chain_id, network) or value.get("rpc")
        record: dict = {
            "axelar_id": chain_axelar_id(chain_id, value),
            "kind": kind,
            "its_address": its["address"],
            "rpc": rpc,
            "trusted": None,
            "error": None,
        }
        out[chain_id] = record
    return out


def query_trust_lists(
    chains: dict[str, dict],
    other_axelar_ids: list[str],
) -> None:
    """Populate the `trusted` field for each chain in-place."""
    for chain_id, rec in chains.items():
        rpc = rec["rpc"]
        kind = rec["kind"]
        if not rpc:
            rec["error"] = "no rpc in config"
            continue

        if kind == "evm":
            if not evm_rpc_alive(rpc):
                rec["error"] = "RPC unresponsive (eth_chainId probe failed)"
                rec["trusted"] = None
                rec["unknown"] = list(other_axelar_ids)
                continue
            trusted: list[str] = []
            unknown: list[str] = []
            for cand in other_axelar_ids:
                if cand == rec["axelar_id"]:
                    continue
                tr = evm_query_trust(rpc, rec["its_address"], cand)
                if tr is True:
                    trusted.append(cand)
                elif tr is None:
                    unknown.append(cand)
            rec["trusted"] = trusted
            rec["unknown"] = unknown
            if unknown:
                rec["error"] = (
                    f"{len(unknown)}/{len(other_axelar_ids) - 1} eth_call(s) failed "
                    f"(public RPC flakiness — retry or use a private endpoint)"
                )

        elif kind == "solana":
            try:
                rec["trusted"] = solana_fetch_trusted_chains(rpc, rec["its_address"])
                rec["unknown"] = []
            except (RuntimeError, urllib.error.URLError, ValueError, KeyError) as e:
                rec["error"] = str(e)

        elif kind in ("sui", "stellar", "xrpl", "unknown"):
            rec["error"] = f"{kind}: trust query not implemented in this script yet"


# ---------------------------------------------------------------------------
# Reporting
# ---------------------------------------------------------------------------


def print_section(title: str) -> None:
    print()
    print("=" * 72)
    print(title)
    print("=" * 72)


def report_per_chain(chains: dict[str, dict]) -> None:
    print_section("Per-chain trust lists")
    for chain_id in sorted(chains):
        rec = chains[chain_id]
        header = f"{chain_id}  ({rec['kind']}, axelar_id={rec['axelar_id']})"
        print(f"\n[{header}]")
        if rec["error"] and rec["trusted"] is None:
            print(f"  ! {rec['error']}")
            continue
        trusted = rec["trusted"] or []
        if not trusted:
            print("  (no trusted chains)")
        else:
            for t in sorted(trusted):
                print(f"  ✓ {t}")
        if rec["error"]:
            print(f"  ! partial: {rec['error']}")


def report_focus(chains: dict[str, dict], focus: str) -> None:
    """Show which chains trust / don't trust the focus chain (as src/dst)."""
    print_section(f"Trust against '{focus}'")
    # Resolve focus to its actual axelar_id if user passed chain_id
    focus_axelar = focus
    for chain_id, rec in chains.items():
        if chain_id == focus or rec["axelar_id"] == focus:
            focus_axelar = rec["axelar_id"]
            break
    print(f"  (looking for axelar_id = '{focus_axelar}')\n")
    yes, no, unknown = [], [], []
    for chain_id in sorted(chains):
        rec = chains[chain_id]
        if rec["axelar_id"] == focus_axelar:
            continue
        if rec.get("trusted") is None:
            unknown.append(chain_id)
            continue
        if focus_axelar in rec["trusted"]:
            yes.append(chain_id)
        elif focus_axelar in rec.get("unknown", []):
            unknown.append(chain_id)
        else:
            no.append(chain_id)
    print(f"  Chains that DO trust '{focus_axelar}': {len(yes)}")
    for c in yes:
        print(f"    ✓ {c}")
    print(f"\n  Chains that DO NOT trust '{focus_axelar}': {len(no)}")
    for c in no:
        print(f"    ✗ {c}")
    if unknown:
        print(f"\n  Chains where the query was inconclusive: {len(unknown)}")
        for c in unknown:
            err = chains[c].get("error") or "RPC failed"
            print(f"    ? {c}  ({err})")


def report_asymmetric(chains: dict[str, dict]) -> None:
    """List pairs where one side trusts the other but not vice versa."""
    print_section("Asymmetric trust pairs (one-way only)")
    known = {cid: rec for cid, rec in chains.items() if rec["trusted"] is not None}
    by_axelar = {rec["axelar_id"]: cid for cid, rec in known.items()}

    pairs: list[tuple[str, str]] = []
    for cid_x, rec_x in known.items():
        for y_axelar in rec_x["trusted"]:
            cid_y = by_axelar.get(y_axelar)
            if cid_y is None or cid_y == cid_x:
                continue
            rec_y = known.get(cid_y)
            if rec_y is None:
                continue
            if rec_x["axelar_id"] not in rec_y["trusted"]:
                pairs.append((cid_x, cid_y))

    if not pairs:
        print("  (none — every trust edge is bidirectional among known chains)")
        return
    print(f"  {len(pairs)} pair(s) where X trusts Y but Y doesn't trust X:\n")
    for x, y in sorted(pairs):
        print(f"    {x:30s} → {y}   (reverse trust missing)")


def main() -> None:
    args = parse_args()
    config, network = load_config(args)
    print(f"network: {network}", file=sys.stderr)
    print("enumerating chains...", file=sys.stderr)

    chains = gather_trust_lists(config, network)
    print(f"  found {len(chains)} chains with InterchainTokenService", file=sys.stderr)

    # The candidate axelar-id set we query EVM chains against = every chain's
    # axelar_id, including chains we couldn't fetch for ourselves (since EVM
    # ITS trust is a bool per chain name, regardless of who else we audit).
    all_axelar_ids = sorted({rec["axelar_id"] for rec in chains.values()})
    print(f"  candidate trust strings: {len(all_axelar_ids)}", file=sys.stderr)
    print("querying on-chain trust state...", file=sys.stderr)
    query_trust_lists(chains, all_axelar_ids)
    print("done querying.\n", file=sys.stderr)

    report_per_chain(chains)
    report_focus(chains, args.focus)
    report_asymmetric(chains)


if __name__ == "__main__":
    main()
