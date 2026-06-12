//! Solana transaction inspector. Decodes a single signature into a
//! human-readable trace of instructions, inner instructions, and Anchor CPI
//! events. The orchestration lives here; the heavy lifting is in submodules:
//!
//! - [`registry`]: lookup tables for program names, instruction discriminators,
//!   account labels, and event discriminators.
//! - [`format`]: terminal formatting helpers (colors, address rendering,
//!   header printing).
//! - [`parsing`]: borsh + payload decoding primitives.
//! - [`events`]: per-event borsh decoders + dispatch.
//! - [`instructions`]: top-level and inner instruction printers, per-arm
//!   argument decoders, and the JSON projection used by `decode_sol_activity`.

mod events;
mod format;
mod instructions;
mod parsing;
mod registry;

pub use instructions::decode_instruction_args_json;
pub use registry::{EVENT_IX_TAG_LE, event_name, instruction_name, known_programs};

use eyre::Result;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_transaction_status::option_serializer::OptionSerializer;
use solana_transaction_status::{EncodedConfirmedTransactionWithStatusMeta, UiTransactionEncoding};
use std::str::FromStr;

pub const SOLANA_RPCS: &[(&str, &str)] = &[
    ("devnet", "https://api.devnet.solana.com"),
    ("testnet", "https://api.testnet.solana.com"),
    ("mainnet", "https://api.mainnet-beta.solana.com"),
];

pub async fn run(txid: &str, config_rpcs: &[(String, String)]) -> Result<()> {
    let sig =
        Signature::from_str(txid).map_err(|e| eyre::eyre!("invalid Solana signature: {e}"))?;

    let (network, tx_data) = try_fetch_transaction_from_any_network(&sig, config_rpcs)?;

    let slot = tx_data.slot;
    let block_time = tx_data.block_time.unwrap_or(0);

    let meta = tx_data
        .transaction
        .meta
        .as_ref()
        .ok_or_else(|| eyre::eyre!("transaction has no metadata"))?;

    let all_keys = collect_all_account_keys(&tx_data, meta);
    let known = registry::known_programs();

    format::print_tx_header(&network, txid, slot, block_time, meta);

    if let solana_transaction_status::EncodedTransaction::Json(ui_tx) =
        &tx_data.transaction.transaction
        && let solana_transaction_status::UiMessage::Raw(raw) = &ui_tx.message
    {
        instructions::print_top_level_instructions(&raw.instructions, &all_keys, &known);
    }

    if let OptionSerializer::Some(inner_ixs) = &meta.inner_instructions {
        instructions::print_inner_instructions(inner_ixs, &all_keys, &known);
    }

    Ok(())
}

/// Try the config-derived RPCs first (better rate limits, private endpoints),
/// then the public cluster fallbacks. Errors only when none have the tx.
fn try_fetch_transaction_from_any_network(
    sig: &Signature,
    config_rpcs: &[(String, String)],
) -> Result<(String, EncodedConfirmedTransactionWithStatusMeta)> {
    let mut seen = std::collections::HashSet::new();
    let candidates = config_rpcs
        .iter()
        .map(|(network, url)| (network.as_str(), url.as_str()))
        .chain(SOLANA_RPCS.iter().copied())
        .filter(|(_, url)| seen.insert(url.to_string()));
    for (network, rpc_url) in candidates {
        let rpc = crate::solana::rpc_client(rpc_url);
        if let Ok(data) = rpc.get_transaction_with_config(
            sig,
            solana_client::rpc_config::RpcTransactionConfig {
                encoding: Some(UiTransactionEncoding::Json),
                commitment: Some(CommitmentConfig::confirmed()),
                max_supported_transaction_version: Some(0),
            },
        ) {
            return Ok((network.to_string(), data));
        }
    }
    Err(eyre::eyre!(
        "transaction not found on any Solana RPC (tried {} config RPC(s) plus public devnet/testnet/mainnet)",
        config_rpcs.len()
    ))
}

/// Concatenate the static account keys with any addresses loaded via ALTs so
/// account-index lookups in the printers find every key.
fn collect_all_account_keys(
    tx_data: &EncodedConfirmedTransactionWithStatusMeta,
    meta: &solana_transaction_status::UiTransactionStatusMeta,
) -> Vec<Pubkey> {
    let account_keys: Vec<Pubkey> = match &tx_data.transaction.transaction {
        solana_transaction_status::EncodedTransaction::Json(ui_tx) => match &ui_tx.message {
            solana_transaction_status::UiMessage::Raw(raw) => raw
                .account_keys
                .iter()
                .filter_map(|k| Pubkey::from_str(k).ok())
                .collect(),
            _ => vec![],
        },
        _ => vec![],
    };

    let mut all_keys = account_keys;
    if let OptionSerializer::Some(loaded) = &meta.loaded_addresses {
        for k in &loaded.writable {
            if let Ok(pk) = Pubkey::from_str(k) {
                all_keys.push(pk);
            }
        }
        for k in &loaded.readonly {
            if let Ok(pk) = Pubkey::from_str(k) {
                all_keys.push(pk);
            }
        }
    }
    all_keys
}
