use eyre::Result;
use owo_colors::OwoColorize;
use serde::Serialize;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::RpcTransactionConfig;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_transaction_status::UiTransactionEncoding;
use std::str::FromStr;

use super::decode_sol_tx;
use crate::cli::{SolNetwork, SolProgram};

// ---------------------------------------------------------------------------
// Program registry: (network, program_type, label, address, rpc_url)
// ---------------------------------------------------------------------------

struct ProgramEntry {
    network: &'static str,
    rpc_url: &'static str,
    program_type: &'static str,
    label: &'static str,
    address: &'static str,
}

const PROGRAMS: &[ProgramEntry] = &[
    // devnet
    ProgramEntry {
        network: "devnet",
        rpc_url: "https://api.devnet.solana.com",
        program_type: "gateway",
        label: "Gateway",
        address: "gtwT4uGVTYSPnTGv6rSpMheyFyczUicxVWKqdtxNGw9",
    },
    ProgramEntry {
        network: "devnet",
        rpc_url: "https://api.devnet.solana.com",
        program_type: "gas-service",
        label: "GasService",
        address: "gasHyxjNZSNsEiMbRLa5JGLCNx1TRsdCy1xwfMBehYB",
    },
    ProgramEntry {
        network: "devnet",
        rpc_url: "https://api.devnet.solana.com",
        program_type: "memo",
        label: "Memo",
        address: "memKnP9ex71TveNFpsFNVqAYGEe1v9uHVsHNdFPW6FY",
    },
    ProgramEntry {
        network: "devnet",
        rpc_url: "https://api.devnet.solana.com",
        program_type: "its",
        label: "ITS",
        address: "itsm3zZhp2oGgEfq7XBu9ojRCYZJnhzecbAEPCrvx2B",
    },
    ProgramEntry {
        network: "devnet",
        rpc_url: "https://api.devnet.solana.com",
        program_type: "its",
        label: "ITS",
        address: "itsYxmqAxNKUL5zaj3fD1K1whuVhqpxKVoiLGie1reF",
    },
    // stagenet (on solana testnet)
    ProgramEntry {
        network: "testnet",
        rpc_url: "https://api.testnet.solana.com",
        program_type: "gateway",
        label: "Gateway",
        address: "gtwYHfHHipAoj8Hfp3cGr3vhZ8f3UtptGCQLqjBkaSZ",
    },
    ProgramEntry {
        network: "testnet",
        rpc_url: "https://api.testnet.solana.com",
        program_type: "gas-service",
        label: "GasService",
        address: "gasgy6jz24wrfZL98uMy8QFUFziVPZ3bNLGXqnyTstW",
    },
    ProgramEntry {
        network: "testnet",
        rpc_url: "https://api.testnet.solana.com",
        program_type: "memo",
        label: "Memo",
        address: "mem4E22pPgkbHAvoUYHa7HybBgUKn6jFjvj1YnPdkaq",
    },
    ProgramEntry {
        network: "testnet",
        rpc_url: "https://api.testnet.solana.com",
        program_type: "its",
        label: "ITS",
        address: "itsm3zZhp2oGgEfq7XBu9ojRCYZJnhzecbAEPCrvx2B",
    },
    // testnet (on solana devnet)
    ProgramEntry {
        network: "devnet",
        rpc_url: "https://api.devnet.solana.com",
        program_type: "gateway",
        label: "Gateway",
        address: "gtwJ8LWDRWZpbvCqp8sDeTgy3GSyuoEsiaKC8wSXJqq",
    },
    ProgramEntry {
        network: "devnet",
        rpc_url: "https://api.devnet.solana.com",
        program_type: "gas-service",
        label: "GasService",
        address: "gasq7KHHv9Rs8C82hu3dgoBD9wk5LTKpWqbdf5o5juu",
    },
    ProgramEntry {
        network: "devnet",
        rpc_url: "https://api.devnet.solana.com",
        program_type: "memo",
        label: "Memo",
        address: "mem7UJouaeyTgySvXhQSxWtGFrWPQ89jywjc8YvQFRT",
    },
    ProgramEntry {
        network: "devnet",
        rpc_url: "https://api.devnet.solana.com",
        program_type: "its",
        label: "ITS",
        address: "itsJo4kNJ3mdh3requwbtTTt7vyYTudp1pxhn2KiHMc",
    },
];

// ---------------------------------------------------------------------------
// JSON output struct
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ActivityEntry {
    network: String,
    program: String,
    program_address: String,
    signature: String,
    slot: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<i64>,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    instruction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    args: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    events: Vec<String>,
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

pub async fn run(
    program_filter: Option<SolProgram>,
    network_filter: Option<SolNetwork>,
    limit: usize,
    json_mode: bool,
) -> Result<()> {
    let network_str = network_filter.map(|n| match n {
        SolNetwork::Devnet => "devnet",
        SolNetwork::Testnet => "testnet",
        SolNetwork::Mainnet => "mainnet",
    });

    let program_str = program_filter.map(|p| match p {
        SolProgram::Gateway => "gateway",
        SolProgram::Its => "its",
        SolProgram::GasService => "gas-service",
        SolProgram::Memo => "memo",
    });

    let filtered: Vec<&ProgramEntry> = PROGRAMS
        .iter()
        .filter(|p| network_str.is_none() || network_str == Some(p.network))
        .filter(|p| program_str.is_none() || program_str == Some(p.program_type))
        .collect();

    if filtered.is_empty() {
        return Err(eyre::eyre!("no programs match the given filters"));
    }

    let known = decode_sol_tx::known_programs();
    let mut all_entries: Vec<ActivityEntry> = Vec::new();

    for entry in &filtered {
        let pubkey = match Pubkey::from_str(entry.address) {
            Ok(pk) => pk,
            Err(_) => continue,
        };

        let rpc = RpcClient::new_with_commitment(entry.rpc_url, CommitmentConfig::confirmed());

        let sigs = match rpc.get_signatures_for_address_with_config(
            &pubkey,
            solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config {
                limit: Some(limit),
                ..Default::default()
            },
        ) {
            Ok(s) => s,
            Err(_) => continue,
        };

        if sigs.is_empty() {
            continue;
        }

        if !json_mode {
            println!(
                "\n{}",
                format!(
                    "━━ {} ({}) {} ━━",
                    entry.label, entry.network, entry.address
                )
                .bold()
            );
        }

        for sig_info in &sigs {
            let status = if sig_info.err.is_some() {
                "Failed"
            } else {
                "Success"
            };

            let sig = &sig_info.signature;
            let slot = sig_info.slot;
            let block_time = sig_info.block_time;

            // Fetch full tx to identify instruction
            let (ix_name, args_json, events) = fetch_and_decode(&rpc, sig, &known);

            if !json_mode {
                let time_str = block_time
                    .map(format_timestamp)
                    .unwrap_or_else(|| "?".to_string());

                let status_colored = if status == "Success" {
                    format!("{}", "OK".green())
                } else {
                    format!("{}", "FAIL".red())
                };

                let ix_display = ix_name.as_deref().unwrap_or("?");

                let sig_short = if sig.len() > 20 {
                    format!("{}...", &sig[..20])
                } else {
                    sig.clone()
                };

                let events_str = if events.is_empty() {
                    String::new()
                } else {
                    format!(" → {}", events.join(", ").dimmed())
                };

                println!(
                    "  {} [{}] {:<45} {}{}",
                    time_str.dimmed(),
                    status_colored,
                    ix_display.bold(),
                    sig_short.dimmed(),
                    events_str,
                );
            }

            all_entries.push(ActivityEntry {
                network: entry.network.to_string(),
                program: entry.label.to_string(),
                program_address: entry.address.to_string(),
                signature: sig.clone(),
                slot,
                timestamp: block_time,
                status: status.to_string(),
                instruction: ix_name,
                args: args_json,
                events,
            });
        }
    }

    if json_mode {
        println!("{}", serde_json::to_string_pretty(&all_entries)?);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fetch_and_decode(
    rpc: &RpcClient,
    sig_str: &str,
    known: &std::collections::HashMap<Pubkey, &'static str>,
) -> (Option<String>, Option<serde_json::Value>, Vec<String>) {
    let sig = match Signature::from_str(sig_str) {
        Ok(s) => s,
        Err(_) => return (None, None, vec![]),
    };

    let tx = match rpc.get_transaction_with_config(
        &sig,
        RpcTransactionConfig {
            encoding: Some(UiTransactionEncoding::Json),
            commitment: Some(CommitmentConfig::confirmed()),
            max_supported_transaction_version: Some(0),
        },
    ) {
        Ok(t) => t,
        Err(_) => return (None, None, vec![]),
    };

    // Extract account keys
    let account_keys: Vec<Pubkey> = match &tx.transaction.transaction {
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

    // Also get loaded addresses (ALTs)
    let mut all_keys = account_keys.clone();
    if let Some(meta) = &tx.transaction.meta
        && let solana_transaction_status::option_serializer::OptionSerializer::Some(loaded) =
            &meta.loaded_addresses
    {
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

    // Find the primary Axelar instruction (skip ComputeBudget)
    let mut ix_name: Option<String> = None;
    let mut args_json: Option<serde_json::Value> = None;

    if let solana_transaction_status::EncodedTransaction::Json(ui_tx) = &tx.transaction.transaction
        && let solana_transaction_status::UiMessage::Raw(raw) = &ui_tx.message
    {
        for ix in &raw.instructions {
            let program_idx = ix.program_id_index as usize;
            let program_id = all_keys.get(program_idx);
            let program_label = program_id.and_then(|pk| known.get(pk)).copied();

            if program_label == Some("ComputeBudget") {
                continue;
            }

            if program_label.is_some() {
                let data_bytes = bs58::decode(&ix.data).into_vec().unwrap_or_default();
                if let Some(name) = decode_sol_tx::instruction_name(&data_bytes) {
                    args_json = Some(decode_sol_tx::decode_instruction_args_json(
                        name,
                        &data_bytes,
                    ));
                    ix_name = Some(name.to_string());
                    break;
                }
            }
        }
    }

    // Extract events from inner instructions
    let mut events = Vec::new();
    if let Some(meta) = &tx.transaction.meta
        && let solana_transaction_status::option_serializer::OptionSerializer::Some(inner_ixs) =
            &meta.inner_instructions
    {
        for group in inner_ixs {
            for ix in &group.instructions {
                if let solana_transaction_status::UiInstruction::Compiled(ci) = ix {
                    let data_bytes = bs58::decode(&ci.data).into_vec().unwrap_or_default();
                    if data_bytes.len() >= 16
                        && data_bytes[..8] == *decode_sol_tx::EVENT_IX_TAG_LE
                        && let Some(name) = decode_sol_tx::event_name(&data_bytes[8..16])
                    {
                        events.push(name.to_string());
                    }
                }
            }
        }
    }

    (ix_name, args_json, events)
}

fn format_timestamp(ts: i64) -> String {
    let secs = ts;
    let days_since_epoch = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;

    // Simple date calculation (good enough for display)
    let mut y = 1970i64;
    let mut remaining_days = days_since_epoch;
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if remaining_days < days_in_year {
            break;
        }
        remaining_days -= days_in_year;
        y += 1;
    }
    let month_days = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 0usize;
    for (i, &d) in month_days.iter().enumerate() {
        if remaining_days < d {
            m = i;
            break;
        }
        remaining_days -= d;
    }

    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        y,
        m + 1,
        remaining_days + 1,
        hours,
        minutes
    )
}
