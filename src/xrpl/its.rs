//! Axelar ITS memo construction for XRPL `Payment` transactions.

use xrpl_types::{Blob, Memo};

/// Build an XRPL `Memo` from a UTF-8 key and arbitrary bytes value.
pub(super) fn memo(key: &str, value: impl AsRef<[u8]>) -> Memo {
    Memo {
        memo_type: Blob(key.as_bytes().to_vec()),
        memo_data: Blob(value.as_ref().to_vec()),
        memo_format: None,
    }
}

/// Build the 4 Axelar ITS `interchain_transfer` memos for a native-XRP payment
/// to the Axelar multisig.
///
/// * `destination_chain` — e.g. `"xrpl-evm"`
/// * `destination_address_hex` — hex-encoded destination bytes, WITHOUT
///   the leading `0x` (this matches the off-chain TypeScript reference
///   implementation in `axelar-contract-deployments/xrpl/interchain-transfer.js`)
/// * `gas_fee_drops` — gas fee, in the same units as the payment `Amount`
///   (drops for XRP), encoded as a decimal string
pub fn build_its_transfer_memos(
    destination_chain: &str,
    destination_address_hex: &str,
    gas_fee_drops: u64,
    payload: Option<&[u8]>,
) -> Vec<Memo> {
    let mut memos = vec![
        memo("type", b"interchain_transfer"),
        memo("destination_address", destination_address_hex.as_bytes()),
        memo("destination_chain", destination_chain.as_bytes()),
        memo("gas_fee_amount", gas_fee_drops.to_string().as_bytes()),
    ];
    if let Some(p) = payload {
        memos.push(memo("payload", p));
    }
    memos
}
