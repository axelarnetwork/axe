//! Small free-floating XRPL helpers: address parsing/formatting, faucet URL
//! lookup, signed-transaction hashing, and the drops/XRP unit conversion.

use eyre::{Result, eyre};
use xrpl_types::AccountId;

/// Drops per XRP. Used by the `xrp_to_drops` helper.
const DROPS_PER_XRP: u64 = 1_000_000;

/// Parse an r-address string into an `AccountId`.
pub fn parse_address(addr: &str) -> Result<AccountId> {
    AccountId::from_address(addr).map_err(|e| eyre!("invalid XRPL address {addr:?}: {e}"))
}

/// Encode an `AccountId`'s 20-byte payload as lowercase hex (no 0x prefix).
/// Used when building the `destination_address` memo for inbound transfers
/// where the destination is an XRPL account.
pub fn account_id_to_hex(id: &AccountId) -> String {
    hex::encode(id.0)
}

/// Convenience conversion: 1 XRP = 1_000_000 drops.
pub const fn xrp_to_drops(xrp: u64) -> u64 {
    xrp.saturating_mul(DROPS_PER_XRP)
}

/// Default faucet URL for a given XRPL chain. We look at the configured
/// RPC/WSS URL because the chain config's `networkType` is unreliable on
/// devnet-amplifier (it labels the chain "testnet" even though the multisig
/// lives on XRPL devnet — a separate ledger). Returns `None` for mainnet.
pub fn faucet_url_for_network(network_type_or_rpc: &str) -> Option<&'static str> {
    let lower = network_type_or_rpc.to_lowercase();
    if lower.contains("devnet") {
        Some("https://faucet.devnet.rippletest.net/accounts")
    } else if lower.contains("altnet") || lower == "testnet" || lower == "stagenet" {
        Some("https://faucet.altnet.rippletest.net/accounts")
    } else {
        None
    }
}

/// Compute the deterministic hash of a signed XRPL transaction blob, returning
/// it as 64 uppercase hex characters (the canonical XRPL tx hash format).
pub(super) fn signed_tx_hash_hex(tx_bytes: &[u8]) -> String {
    let h = xrpl_binary_codec::hash::hash(
        xrpl_binary_codec::hash::HASH_PREFIX_SIGNED_TRANSACTION,
        tx_bytes,
    );
    h.to_hex()
}
