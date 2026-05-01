use std::path::{Path, PathBuf};

use comfy_table::{Cell, ContentArrangement, Table};
use eyre::Result;
use serde_json::{Value, json};

use crate::commands::verifiers::lookup_name;
use crate::cosmos::{read_axelar_contract_field, rpc_block_time, rpc_tx_search};
use crate::ui;

const SUPPORTED_NETWORKS: &[crate::types::Network] = &[
    crate::types::Network::Testnet,
    crate::types::Network::Mainnet,
];

fn resolve_config(network: crate::types::Network) -> Result<PathBuf> {
    let config_dir = PathBuf::from("../axelar-contract-deployments/axelar-chains-config/info");
    let path = config_dir.join(format!("{network}.json"));
    if !path.exists() {
        return Err(eyre::eyre!(
            "config not found for network '{}' at {}. Make sure axelar-contract-deployments is a sibling directory.",
            network,
            path.display()
        ));
    }
    Ok(path)
}

fn resolve_chain_axelar_id(config_path: &Path, chain_input: &str) -> Result<String> {
    let content = std::fs::read_to_string(config_path)?;
    let root: Value = serde_json::from_str(&content)?;
    let chains = root
        .get("chains")
        .and_then(|v| v.as_object())
        .ok_or_else(|| eyre::eyre!("no 'chains' in config"))?;

    if let Some(chain_config) = chains.get(chain_input) {
        return Ok(chain_config
            .get("axelarId")
            .and_then(|v| v.as_str())
            .unwrap_or(chain_input)
            .to_string());
    }
    for (key, chain_config) in chains {
        let axelar_id = chain_config
            .get("axelarId")
            .and_then(|v| v.as_str())
            .unwrap_or(key);
        if axelar_id.eq_ignore_ascii_case(chain_input) {
            return Ok(axelar_id.to_string());
        }
    }
    let mut available: Vec<&str> = chains.keys().map(|k| k.as_str()).collect();
    available.sort();
    Err(eyre::eyre!(
        "chain '{}' not found in config. Available: {}",
        chain_input,
        available.join(", ")
    ))
}

fn read_axelar_rpc_from(config_path: &Path) -> Result<String> {
    let content = std::fs::read_to_string(config_path)?;
    let root: Value = serde_json::from_str(&content)?;
    root.pointer("/axelar/rpc")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| eyre::eyre!("no axelar.rpc in config"))
}

#[derive(Debug)]
struct VoteRow {
    height: u64,
    tx_hash: String,
    poll_id: String,
    votes: Vec<String>, // raw vote strings: "succeeded_on_chain", "failed_on_chain", "not_found"
}

/// Translate a single vote string to a short label.
fn vote_label(v: &str) -> &'static str {
    match v {
        "succeeded_on_chain" => "Y",
        "failed_on_chain" => "F",
        "not_found" => "?",
        _ => "X",
    }
}

/// Format an ISO-8601 timestamp as a compact "X ago" string relative to now.
/// Returns "-" if the timestamp can't be parsed.
fn relative_time(iso: &str) -> String {
    use chrono::{DateTime, Utc};
    let parsed: Option<DateTime<Utc>> = iso.parse::<DateTime<Utc>>().ok().or_else(|| {
        DateTime::parse_from_rfc3339(iso)
            .ok()
            .map(|d| d.with_timezone(&Utc))
    });
    let Some(t) = parsed else {
        return "-".to_string();
    };
    let now = Utc::now();
    let secs = (now - t).num_seconds();
    if secs < 0 {
        return "now".to_string();
    }
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        let m = mins % 60;
        return if m == 0 {
            format!("{hours}h ago")
        } else {
            format!("{hours}h {m}m ago")
        };
    }
    let days = hours / 24;
    if days < 30 {
        let h = hours % 24;
        return if h == 0 {
            format!("{days}d ago")
        } else {
            format!("{days}d {h}h ago")
        };
    }
    let months = days / 30;
    let d = days % 30;
    if d == 0 {
        format!("{months}mo ago")
    } else {
        format!("{months}mo {d}d ago")
    }
}

/// Summarise a votes vector into a compact label (`Y`, `F`, `?`, or e.g. `Y,Y,F`).
fn vote_summary(votes: &[String]) -> String {
    if votes.is_empty() {
        return "-".to_string();
    }
    if votes.iter().all(|v| v == &votes[0]) {
        return vote_label(&votes[0]).to_string();
    }
    votes
        .iter()
        .map(|v| vote_label(v))
        .collect::<Vec<_>>()
        .join(",")
}

pub async fn run(
    network: String,
    chain: String,
    verifier: String,
    limit: usize,
    json_mode: bool,
) -> Result<()> {
    let network: crate::types::Network = network.parse()?;
    if !SUPPORTED_NETWORKS.contains(&network) {
        return Err(eyre::eyre!(
            "verifier-votes only supports: {}",
            SUPPORTED_NETWORKS
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let config_path = resolve_config(network)?;
    let chain_axelar_id = resolve_chain_axelar_id(&config_path, &chain)?;
    let rpc = read_axelar_rpc_from(&config_path)?;

    let vv_pointer = format!("/axelar/contracts/VotingVerifier/{chain_axelar_id}/address");
    let vv_addr = read_axelar_contract_field(&config_path, &vv_pointer).map_err(|_| {
        eyre::eyre!(
            "no VotingVerifier address for chain '{chain_axelar_id}' on {network}. Is it Amplifier?"
        )
    })?;

    let display_name = lookup_name(network, &verifier);
    let verifier_display = match display_name {
        Some(name) => format!("{name} ({verifier})"),
        None => verifier.clone(),
    };

    if !json_mode {
        ui::section(&format!(
            "Verifier votes: {network} / {chain_axelar_id} / {verifier_display}"
        ));
    }

    // Walk pages of wasm-voted events for this voter on this voting-verifier.
    let filter = format!(
        "wasm-voted.voter='{}' AND wasm-voted._contract_address='{}'",
        verifier, vv_addr
    );

    let spinner = ui::wait_spinner("querying tx_search...");
    let mut rows: Vec<VoteRow> = Vec::new();
    'outer: for page in 1..=20u32 {
        let res = rpc_tx_search(&rpc, &filter, 100, page, true).await?;
        let txs = res.get("txs").and_then(|v| v.as_array()).cloned();
        let txs = match txs {
            Some(t) if !t.is_empty() => t,
            _ => break,
        };
        for t in &txs {
            let height: u64 = t
                .get("height")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let hash = t
                .get("hash")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let events = t
                .pointer("/tx_result/events")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            for e in &events {
                if e.get("type").and_then(|v| v.as_str()) != Some("wasm-voted") {
                    continue;
                }
                let attrs = e
                    .get("attributes")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut poll_id: Option<String> = None;
                let mut votes_str: Option<String> = None;
                let mut voter: Option<String> = None;
                let mut contract_addr: Option<String> = None;
                for a in &attrs {
                    let key = a.get("key").and_then(|v| v.as_str()).unwrap_or("");
                    let val = a.get("value").and_then(|v| v.as_str()).unwrap_or("");
                    match key {
                        "poll_id" => poll_id = Some(val.trim_matches('"').to_string()),
                        "votes" => votes_str = Some(val.to_string()),
                        "voter" => voter = Some(val.to_string()),
                        "_contract_address" => contract_addr = Some(val.to_string()),
                        _ => {}
                    }
                }
                if voter.as_deref() != Some(verifier.as_str()) {
                    continue;
                }
                if contract_addr.as_deref() != Some(vv_addr.as_str()) {
                    continue;
                }
                let votes: Vec<String> = votes_str
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
                    .unwrap_or_default();
                if let Some(pid) = poll_id {
                    rows.push(VoteRow {
                        height,
                        tx_hash: hash.clone(),
                        poll_id: pid,
                        votes,
                    });
                    if rows.len() >= limit {
                        break 'outer;
                    }
                }
            }
        }
        if txs.len() < 100 {
            break;
        }
    }
    spinner.finish_and_clear();

    if rows.is_empty() {
        return Err(eyre::eyre!(
            "no wasm-voted events found for {} on {} (voting-verifier {}).",
            verifier,
            chain_axelar_id,
            vv_addr
        ));
    }

    // Best-effort: fetch one block timestamp per unique height. Cap to avoid abuse.
    let mut timestamps = std::collections::HashMap::<u64, String>::new();
    let mut heights: Vec<u64> = rows.iter().map(|r| r.height).collect();
    heights.sort();
    heights.dedup();
    let spinner = ui::wait_spinner("fetching block timestamps...");
    for h in heights.iter().take(60) {
        if let Ok(t) = rpc_block_time(&rpc, *h).await {
            timestamps.insert(*h, t);
        }
    }
    spinner.finish_and_clear();

    if json_mode {
        let entries = json!({
            "verifier": verifier,
            "name": display_name,
            "voting_verifier": vv_addr,
            "chain": chain_axelar_id,
            "votes": rows
                .iter()
                .map(|r| {
                    json!({
                        "height": r.height,
                        "tx_hash": r.tx_hash,
                        "time": timestamps.get(&r.height).cloned().unwrap_or_default(),
                        "poll_id": r.poll_id,
                        "votes": r.votes,
                        "summary": vote_summary(&r.votes),
                    })
                })
                .collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }

    let mut table = Table::new();
    table.load_preset(comfy_table::presets::UTF8_FULL_CONDENSED);
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec![
        Cell::new("#"),
        Cell::new("Height"),
        Cell::new("When"),
        Cell::new("Poll"),
        Cell::new("#Msgs"),
        Cell::new("Vote"),
    ]);
    let mut row_num = 0usize;
    for r in &rows {
        row_num += 1;
        let when = timestamps
            .get(&r.height)
            .map(|s| relative_time(s))
            .unwrap_or_else(|| "-".to_string());
        table.add_row(vec![
            Cell::new(row_num),
            Cell::new(r.height),
            Cell::new(when),
            Cell::new(&r.poll_id),
            Cell::new(r.votes.len()),
            Cell::new(vote_summary(&r.votes)),
        ]);
    }
    println!();
    println!("{table}");

    let yes = rows
        .iter()
        .filter(|r| r.votes.iter().all(|v| v == "succeeded_on_chain"))
        .count();
    let nos = rows.len() - yes;
    println!();
    ui::kv("verifier", &verifier_display);
    ui::kv("voting-verifier", &format!("{vv_addr} ({chain_axelar_id})"));
    ui::kv(
        "showing",
        &format!("{} most recent votes (limit={})", rows.len(), limit),
    );
    ui::kv(
        "summary",
        &format!("{yes} all-yes, {nos} contained any non-yes"),
    );

    Ok(())
}
