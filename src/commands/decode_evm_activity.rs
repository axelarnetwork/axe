use alloy::primitives::{Address, B256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Filter;
use eyre::Result;
use owo_colors::OwoColorize;
use serde::Serialize;
use std::path::PathBuf;
use std::str::FromStr;

use super::decode;
use crate::cli::EvmContract;

// ---------------------------------------------------------------------------
// Contract discovery from config files
// ---------------------------------------------------------------------------

struct EvmContractEntry {
    network: String,
    chain_name: String,
    rpc_url: String,
    _contract_type: String,
    label: String,
    address: Address,
}

fn discover_contracts(
    network: &str,
    chain: &str,
    contract_filter: Option<EvmContract>,
) -> Vec<EvmContractEntry> {
    let config_dir = PathBuf::from("../axelar-contract-deployments/axelar-chains-config/info");
    let networks = vec![network.to_string()];

    let type_filter = contract_filter.map(|c| match c {
        EvmContract::Gateway => "gateway",
        EvmContract::Its => "its",
        EvmContract::GasService => "gas-service",
    });

    let mut entries = Vec::new();

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

        for (chain_name, chain_config) in chains {
            let chain_type = chain_config
                .get("chainType")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if chain_type != "evm" {
                continue;
            }

            if chain_name != chain {
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

            let contract_map = [
                ("AxelarGateway", "gateway", "Gateway"),
                ("InterchainTokenService", "its", "ITS"),
                ("AxelarGasService", "gas-service", "GasService"),
            ];

            for (contract_name, prog_type, label) in &contract_map {
                if let Some(filter) = type_filter
                    && *prog_type != filter
                {
                    continue;
                }

                let addr_str = contracts
                    .get(*contract_name)
                    .and_then(|v| v.get("address").or(Some(v)))
                    .and_then(|v| v.as_str());

                if let Some(addr_str) = addr_str
                    && let Ok(address) = Address::from_str(addr_str)
                {
                    entries.push(EvmContractEntry {
                        network: network.clone(),
                        chain_name: chain_name.clone(),
                        rpc_url: rpc_url.clone(),
                        _contract_type: prog_type.to_string(),
                        label: label.to_string(),
                        address,
                    });
                }
            }
        }
    }

    entries
}

// ---------------------------------------------------------------------------
// JSON output
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct EvmActivityEntry {
    network: String,
    chain: String,
    contract: String,
    contract_address: String,
    event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tx_hash: Option<String>,
    block: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    log_index: Option<u64>,
    params: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

pub async fn run(
    contract_filter: Option<EvmContract>,
    network: String,
    chain: String,
    limit: usize,
    json_mode: bool,
) -> Result<()> {
    let contracts = discover_contracts(&network, &chain, contract_filter);

    if contracts.is_empty() {
        return Err(eyre::eyre!(
            "no EVM contracts found. Make sure axelar-contract-deployments is a sibling directory."
        ));
    }

    let mut all_entries: Vec<EvmActivityEntry> = Vec::new();

    for entry in &contracts {
        let provider = ProviderBuilder::new().connect_http(entry.rpc_url.parse()?);

        let latest_block = match provider.get_block_number().await {
            Ok(b) => b,
            Err(_) => {
                if !json_mode {
                    eprintln!(
                        "  {} could not reach {} ({})",
                        "!".yellow(),
                        entry.chain_name,
                        entry.rpc_url
                    );
                }
                continue;
            }
        };

        let from_block = latest_block.saturating_sub(10000);

        let filter = Filter::new()
            .address(entry.address)
            .from_block(from_block)
            .to_block(latest_block);

        let logs = match provider.get_logs(&filter).await {
            Ok(l) => l,
            Err(_) => continue,
        };

        if logs.is_empty() {
            continue;
        }

        // Take the last N logs (most recent)
        let recent_logs: Vec<_> = if logs.len() > limit {
            logs[logs.len() - limit..].to_vec()
        } else {
            logs
        };

        if !json_mode {
            let addr_short = format!(
                "0x{}...{}",
                &format!("{:x}", entry.address)[..4],
                &format!("{:x}", entry.address)[36..]
            );
            println!(
                "\n{}",
                format!(
                    "━━ {} ({}/{}) {} ━━",
                    entry.label, entry.network, entry.chain_name, addr_short
                )
                .bold()
            );
        }

        for log in &recent_logs {
            let topics: Vec<B256> = log.topics().to_vec();
            let data = log.data().data.as_ref();

            let (event_name, params) = match decode::decode_log(&topics, data) {
                Some((name, params)) => (name, params),
                None => {
                    let topic0 = topics
                        .first()
                        .map(|t| format!("0x{t:.8}..."))
                        .unwrap_or_default();
                    (format!("Unknown({topic0})"), vec![])
                }
            };

            // Extract just the event name (before the parentheses)
            let short_name = event_name.split('(').next().unwrap_or(&event_name);

            let tx_hash = log.transaction_hash.map(|h| format!("0x{h:x}"));
            let block_num = log.block_number.unwrap_or(0);
            let log_index = log.log_index;

            // Build params summary for human mode
            let params_summary = build_params_summary(short_name, &params);

            // Build JSON params
            let params_json = params_to_json(&params);

            if !json_mode {
                let tx_short = tx_hash
                    .as_deref()
                    .map(|h| {
                        if h.len() > 14 {
                            format!("{}...", &h[..14])
                        } else {
                            h.to_string()
                        }
                    })
                    .unwrap_or_default();

                println!(
                    "  {} {:<42} {}  {}",
                    format!("blk {block_num}").dimmed(),
                    short_name.bold(),
                    params_summary.dimmed(),
                    tx_short.dimmed(),
                );
            }

            all_entries.push(EvmActivityEntry {
                network: entry.network.clone(),
                chain: entry.chain_name.clone(),
                contract: entry.label.clone(),
                contract_address: format!("0x{:x}", entry.address),
                event: short_name.to_string(),
                tx_hash,
                block: block_num,
                log_index,
                params: params_json,
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

fn build_params_summary(
    event_name: &str,
    params: &[(String, alloy::dyn_abi::DynSolValue)],
) -> String {
    let get = |name: &str| -> Option<String> {
        params.iter().find(|(n, _)| n == name).map(|(_, v)| {
            let s = format_sol_value(v);
            if s.len() > 20 {
                format!("{}...", &s[..20])
            } else {
                s
            }
        })
    };

    match event_name {
        "ContractCall" => {
            let dest = get("destinationChain").unwrap_or_default();
            let sender = get("sender").unwrap_or_default();
            format!("dest={dest} sender={sender}")
        }
        "InterchainTransfer" => {
            let dest = get("destinationChain").unwrap_or_default();
            let amount = get("amount").unwrap_or_default();
            format!("dest={dest} amount={amount}")
        }
        "InterchainTokenDeploymentStarted" | "InterchainTokenDeployed" => {
            let name = get("tokenName").or_else(|| get("name")).unwrap_or_default();
            let symbol = get("tokenSymbol")
                .or_else(|| get("symbol"))
                .unwrap_or_default();
            format!("{name} ({symbol})")
        }
        "NativeGasPaidForContractCall" | "GasPaidForContractCall" => {
            let dest = get("destinationChain").unwrap_or_default();
            let amount = get("gasFeeAmount").unwrap_or_default();
            format!("dest={dest} fee={amount}")
        }
        "ContractCallApproved" | "MessageApproved" => {
            let src = get("sourceChain").unwrap_or_default();
            format!("source={src}")
        }
        _ => {
            // Show first 2 params
            params
                .iter()
                .take(2)
                .map(|(n, v)| {
                    let s = format_sol_value(v);
                    let short = if s.len() > 16 {
                        format!("{}...", &s[..16])
                    } else {
                        s
                    };
                    format!("{n}={short}")
                })
                .collect::<Vec<_>>()
                .join(" ")
        }
    }
}

fn format_sol_value(value: &alloy::dyn_abi::DynSolValue) -> String {
    use alloy::dyn_abi::DynSolValue;
    match value {
        DynSolValue::String(s) => s.clone(),
        DynSolValue::Address(a) => format!("0x{a:x}"),
        DynSolValue::Uint(u, _) => format!("{u}"),
        DynSolValue::Int(i, _) => format!("{i}"),
        DynSolValue::Bool(b) => format!("{b}"),
        DynSolValue::FixedBytes(b, _) => format!("0x{}", hex::encode(b)),
        DynSolValue::Bytes(b) => {
            if b.len() <= 32 {
                format!("0x{}", hex::encode(b))
            } else {
                format!("0x{}...", hex::encode(&b[..16]))
            }
        }
        _ => format!("{value:?}"),
    }
}

fn params_to_json(params: &[(String, alloy::dyn_abi::DynSolValue)]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (name, value) in params {
        map.insert(name.clone(), sol_value_to_json(value));
    }
    serde_json::Value::Object(map)
}

fn sol_value_to_json(value: &alloy::dyn_abi::DynSolValue) -> serde_json::Value {
    use alloy::dyn_abi::DynSolValue;
    use serde_json::json;

    match value {
        DynSolValue::Bool(b) => json!(b),
        DynSolValue::Uint(u, _) => json!(format!("{u}")),
        DynSolValue::Int(i, _) => json!(format!("{i}")),
        DynSolValue::Address(a) => json!(format!("0x{a:x}")),
        DynSolValue::FixedBytes(b, _) => json!(format!("0x{}", hex::encode(b))),
        DynSolValue::Bytes(b) => {
            if b.len() <= 64 {
                json!(format!("0x{}", hex::encode(b)))
            } else {
                json!(format!(
                    "0x{}... ({} bytes)",
                    hex::encode(&b[..32]),
                    b.len()
                ))
            }
        }
        DynSolValue::String(s) => json!(s),
        DynSolValue::Array(arr) => {
            json!(arr.iter().map(sol_value_to_json).collect::<Vec<_>>())
        }
        DynSolValue::Tuple(arr) => {
            json!(arr.iter().map(sol_value_to_json).collect::<Vec<_>>())
        }
        _ => json!(format!("{value:?}")),
    }
}
