use eyre::Result;
use owo_colors::OwoColorize;
use serde::Serialize;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::RpcTransactionConfig;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_transaction_status::UiTransactionEncoding;
use std::path::PathBuf;
use std::str::FromStr;

use super::decode_sol_tx;
use crate::cli::SolProgram;

// ---------------------------------------------------------------------------
// Config discovery
// ---------------------------------------------------------------------------

struct DiscoveredProgram {
    network: String,
    chain_name: String,
    rpc_url: String,
    _program_type: String,
    label: String,
    address: String,
}

fn discover_programs(
    network_filter: Option<&str>,
    program_filter: Option<SolProgram>,
) -> Vec<DiscoveredProgram> {
    let config_dir = PathBuf::from("../axelar-contract-deployments/axelar-chains-config/info");
    let networks = if let Some(n) = network_filter {
        vec![n.to_string()]
    } else {
        vec![
            "devnet-amplifier".to_string(),
            "stagenet".to_string(),
            "testnet".to_string(),
            "mainnet".to_string(),
        ]
    };

    let program_type_filter = program_filter.map(|p| match p {
        SolProgram::Gateway => "gateway",
        SolProgram::Its => "its",
        SolProgram::GasService => "gas-service",
        SolProgram::Memo => "memo",
    });

    let mut programs = Vec::new();

    for network in &networks {
        let config_path = config_dir.join(format!("{network}.json"));
        let config_content = match std::fs::read_to_string(&config_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let config: serde_json::Value = match serde_json::from_str(&config_content) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let chains = match config.get("chains").and_then(|v| v.as_object()) {
            Some(c) => c,
            None => continue,
        };

        // Find SVM chains and their RPC
        for (chain_name, chain_config) in chains {
            let chain_type = chain_config
                .get("chainType")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if chain_type != "svm" {
                continue;
            }

            let rpc_url = match chain_config.get("rpc").and_then(|v| v.as_str()) {
                Some(r) => r.to_string(),
                None => continue,
            };

            let contracts = match chain_config.get("contracts").and_then(|v| v.as_object()) {
                Some(c) => c,
                None => continue,
            };

            // Map contract names to our program types
            let contract_map = [
                ("AxelarGateway", "gateway", "Gateway"),
                ("AxelarGasService", "gas-service", "GasService"),
                ("AxelarMemo", "memo", "Memo"),
                ("InterchainTokenService", "its", "ITS"),
            ];

            for (contract_name, prog_type, label) in &contract_map {
                if let Some(filter) = program_type_filter
                    && *prog_type != filter
                {
                    continue;
                }

                let address = contracts
                    .get(*contract_name)
                    .and_then(|v| v.get("address").or(Some(v)))
                    .and_then(|v| v.as_str());

                if let Some(addr) = address {
                    programs.push(DiscoveredProgram {
                        network: network.clone(),
                        chain_name: chain_name.clone(),
                        rpc_url: rpc_url.clone(),
                        _program_type: prog_type.to_string(),
                        label: label.to_string(),
                        address: addr.to_string(),
                    });
                }
            }
        }
    }

    programs
}

// ---------------------------------------------------------------------------
// JSON output struct
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ActivityEntry {
    network: String,
    chain: String,
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
    network: Option<String>,
    limit: usize,
    json_mode: bool,
) -> Result<()> {
    let programs = discover_programs(network.as_deref(), program_filter);

    if programs.is_empty() {
        return Err(eyre::eyre!(
            "no Solana programs found. Make sure axelar-contract-deployments is a sibling directory."
        ));
    }

    let known = decode_sol_tx::known_programs();
    let mut all_entries: Vec<ActivityEntry> = Vec::new();

    for entry in &programs {
        let pubkey = match Pubkey::from_str(&entry.address) {
            Ok(pk) => pk,
            Err(_) => continue,
        };

        let rpc = RpcClient::new_with_commitment(&entry.rpc_url, CommitmentConfig::confirmed());

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
                    "━━ {} ({}/{}) {} ━━",
                    entry.label, entry.network, entry.chain_name, entry.address
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
                network: entry.network.clone(),
                chain: entry.chain_name.clone(),
                program: entry.label.clone(),
                program_address: entry.address.clone(),
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
