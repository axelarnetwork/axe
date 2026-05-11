//! Output formatting helpers shared by the inspector. All `dimmed()` /
//! `bold()` / `cyan()` styling lives here so the rest of the module reads as
//! data shaping, not terminal painting.

use owo_colors::OwoColorize;
use solana_sdk::pubkey::Pubkey;
use solana_transaction_status::option_serializer::OptionSerializer;
use std::collections::HashMap;
use std::fmt::Display;

/// Format an account pubkey with its known program name and role label.
pub(super) fn format_account(
    pk: &Pubkey,
    known: &HashMap<Pubkey, &'static str>,
    label: Option<&str>,
) -> String {
    let name = known.get(pk).map(|s| format!(" ({})", s));
    let role = label.map(|l| format!(" ← {}", l.dimmed()));
    format!(
        "{}{}{}",
        pk,
        name.unwrap_or_default(),
        role.unwrap_or_default()
    )
}

/// Format raw address bytes for display: 20 bytes → EVM 0x,
/// 32 bytes → Solana base58, valid ASCII → string, otherwise → 0x hex.
pub(super) fn format_address_bytes(bytes: &[u8]) -> String {
    if bytes.len() == 20 {
        return format!("0x{}", hex::encode(bytes));
    }
    if bytes.len() == 32
        && let Ok(pk) = Pubkey::try_from(bytes)
    {
        return pk.to_string();
    }
    if let Ok(s) = std::str::from_utf8(bytes)
        && s.is_ascii()
        && !s.is_empty()
    {
        return s.to_string();
    }
    format!("0x{}", hex::encode(bytes))
}

/// Build a multi-line `<label>: <value>` block with dimmed labels. Used by
/// the borsh event decoders so each one names its fields once instead of
/// interleaving labels and values into a positional `format!`.
pub(super) fn kv_lines<I, K, V>(rows: I) -> String
where
    I: IntoIterator<Item = (K, V)>,
    K: Display,
    V: Display,
{
    rows.into_iter()
        .map(|(k, v)| format!("{} {v}", format!("{k}:").dimmed()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Print one indented `<label>: <value>` line with the label dimmed. Used by
/// the per-instruction arg printers so each call site reads as data, not as
/// formatting.
pub(super) fn println_kv(indent: &str, label: &str, value: impl Display) {
    println!("{indent}{} {value}", format!("{label}:").dimmed());
}

pub(super) fn print_tx_header(
    network: &str,
    txid: &str,
    slot: u64,
    block_time: i64,
    meta: &solana_transaction_status::UiTransactionStatusMeta,
) {
    let status = if meta.err.is_some() {
        "Failed"
    } else {
        "Success"
    };
    let compute_units = match &meta.compute_units_consumed {
        OptionSerializer::Some(cu) => Some(*cu),
        _ => None,
    };

    println!("{} Solana {}", "Network:".bold(), network.cyan());
    println!("{} {}", "Tx:".bold(), txid);
    println!("{} {}", "Slot:".bold(), slot);
    if block_time > 0 {
        println!("{} {}", "Time:".bold(), block_time);
    }
    println!(
        "{} {}",
        "Status:".bold(),
        if status == "Success" {
            status.green().to_string()
        } else {
            status.red().to_string()
        }
    );
    if let Some(cu) = compute_units {
        println!("{} {}", "Compute Units:".bold(), cu);
    }
    {
        let fee = meta.fee as f64 / 1e9;
        println!("{} ◎{fee}", "Fee:".bold());
    }
}
