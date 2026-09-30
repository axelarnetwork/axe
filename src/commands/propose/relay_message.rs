//! Locate a proposal's governance GMP at its voting deadline, including old runs.
use super::types::{GovMessage, ResolvedConfig};
use crate::{http, ui};
use alloy::primitives::B256;
use eyre::Result;
use serde_json::Value;

pub async fn find_governance_message(
    cfg: &ResolvedConfig,
    payload_hash: B256,
    end: chrono::DateTime<chrono::FixedOffset>,
) -> Result<GovMessage> {
    let rpc = cfg.axelar_rpc.trim_end_matches('/');
    let status: Value = http::get_json(format!("{rpc}/status").parse()?).await?;
    let latest = status
        .pointer("/result/sync_info/latest_block_height")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre::eyre!("missing latest block height"))?
        .parse::<u64>()?;
    let height = execution_height(rpc, latest, end).await?;
    let want = format!("{payload_hash:x}");
    // The gov EndBlocker runs at the first block past the voting deadline.
    for candidate in height.saturating_sub(1)..=latest.min(height + 5) {
        if let Some(msg) = scan_block(cfg, candidate, &want).await? {
            ui::kv("exec block", &candidate.to_string());
            return Ok(msg);
        }
    }
    Err(eyre::eyre!(
        "governance message not found near execution block {height}; rerun to retry, or use an archive RPC if block results were pruned"
    ))
}

async fn execution_height(
    rpc: &str,
    latest: u64,
    end: chrono::DateTime<chrono::FixedOffset>,
) -> Result<u64> {
    let mut upper = latest;
    let mut lower = latest;
    let mut distance = 64_u64;
    while lower > 1 && block_time(rpc, lower).await? >= end {
        upper = lower;
        lower = lower.saturating_sub(distance).max(1);
        distance = distance.saturating_mul(2);
    }
    while lower + 1 < upper {
        let mid = lower + (upper - lower) / 2;
        if block_time(rpc, mid).await? < end {
            lower = mid;
        } else {
            upper = mid;
        }
    }
    Ok(upper)
}

async fn block_time(rpc: &str, height: u64) -> Result<chrono::DateTime<chrono::FixedOffset>> {
    let response: Value = http::get_json(format!("{rpc}/block?height={height}").parse()?).await?;
    let time = response
        .pointer("/result/block/header/time")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            eyre::eyre!("block {height} unavailable; use an archive RPC for older proposals")
        })?;
    Ok(chrono::DateTime::parse_from_rfc3339(time)?)
}

async fn scan_block(
    cfg: &ResolvedConfig,
    height: u64,
    want_hash: &str,
) -> Result<Option<GovMessage>> {
    let url = format!(
        "{}/block_results?height={height}",
        cfg.axelar_rpc.trim_end_matches('/')
    );
    let resp: Value = http::get_json(url.parse()?).await?;
    let events = resp
        .pointer("/result/finalize_block_events")
        .or_else(|| resp.pointer("/finalize_block_events"))
        .and_then(Value::as_array);
    let Some(events) = events else {
        return Ok(None);
    };
    Ok(events
        .iter()
        .filter(|e| {
            e.get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| t.ends_with("contract_called"))
        })
        .find_map(|e| match_contract_called(e, want_hash, cfg)))
}

/// If this `contract_called` event's `payload_hash` matches, pull the fields
/// the relay needs. Attributes are plain strings on CometBFT 0.38.
fn match_contract_called(
    event: &Value,
    want_hash: &str,
    cfg: &ResolvedConfig,
) -> Option<GovMessage> {
    let attrs = event.get("attributes").and_then(Value::as_array)?;
    let get = |key: &str| -> Option<String> {
        attrs
            .iter()
            .find(|a| a.get("key").and_then(Value::as_str) == Some(key))
            .and_then(|a| a.get("value").and_then(Value::as_str))
            .map(str::to_string)
    };
    let hash = get("payload_hash")?;
    if get("_contract_address")? != cfg.axelarnet_gateway
        || get("source_chain")? != "axelar"
        || get("source_address")? != cfg.gov_module
        || get("destination_chain")? != cfg.edge_axelar_id
        || !get("destination_address")?.eq_ignore_ascii_case(&cfg.asg_address)
    {
        return None;
    }
    if !hash
        .trim_start_matches("0x")
        .eq_ignore_ascii_case(want_hash)
    {
        return None;
    }
    Some(GovMessage {
        message_id: get("message_id")?,
        source_chain: get("source_chain")?,
        source_address: get("source_address")?,
    })
}
