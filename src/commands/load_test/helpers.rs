//! Shared helpers used by every per-pair orchestrator: SenderReceiver
//! deploy/reuse, the report finaliser, RPC validation, and the formatted
//! summary block printed at the end of a run.

use std::time::Instant;

use alloy::{
    network::TransactionBuilder,
    primitives::{Address, Bytes},
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    sol_types::SolValue,
};
use eyre::Result;
use owo_colors::OwoColorize;
use serde_json::json;

use super::LoadTestArgs;
use super::metrics::LoadTestReport;
use super::resolve::save_cache;
use crate::evm::{broadcast_and_log, read_artifact_bytecode};
use crate::ui;

/// Deploy or reuse a cached SenderReceiver contract.
pub(super) async fn deploy_or_reuse_sender_receiver<R: Provider, W: Provider>(
    cache: &serde_json::Value,
    cache_key: &str,
    read_provider: &R,
    write_provider: &W,
    gateway_addr: Address,
    gas_service_addr: Address,
    label: &str,
) -> Result<Address> {
    if let Some(addr_str) = cache.get("senderReceiverAddress").and_then(|v| v.as_str()) {
        let addr: Address = addr_str.parse()?;
        let code = read_provider.get_code_at(addr).await?;
        let needs_redeploy = if code.is_empty() {
            ui::warn(&format!(
                "cached SenderReceiver ({label}) has no code, redeploying..."
            ));
            true
        } else {
            // Verify the cached contract's gateway matches the current config.
            let sr = crate::evm::SenderReceiver::new(addr, read_provider);
            match sr.gateway().call().await {
                Ok(onchain_gw) => {
                    if onchain_gw != gateway_addr {
                        ui::warn(&format!(
                            "cached SenderReceiver ({label}) points to old gateway {onchain_gw}, expected {gateway_addr}, redeploying..."
                        ));
                        true
                    } else {
                        false
                    }
                }
                Err(_) => {
                    ui::warn(&format!(
                        "cached SenderReceiver ({label}) gateway check failed, redeploying..."
                    ));
                    true
                }
            }
        };
        if needs_redeploy {
            let new_addr =
                deploy_sender_receiver(write_provider, gateway_addr, gas_service_addr).await?;
            let mut cache = cache.clone();
            cache["senderReceiverAddress"] = json!(format!("{new_addr}"));
            save_cache(cache_key, &cache)?;
            Ok(new_addr)
        } else {
            ui::info(&format!("SenderReceiver ({label}): reusing {addr}"));
            Ok(addr)
        }
    } else {
        ui::info(&format!("deploying SenderReceiver on {label} chain..."));
        let addr = deploy_sender_receiver(write_provider, gateway_addr, gas_service_addr).await?;
        let mut cache = cache.clone();
        cache["senderReceiverAddress"] = json!(format!("{addr}"));
        save_cache(cache_key, &cache)?;
        Ok(addr)
    }
}

pub(super) fn finish_report(
    args: &LoadTestArgs,
    report: &mut LoadTestReport,
    run_start: Instant,
) -> Result<()> {
    report.protocol = format!("{}", args.protocol);
    report.tps = args.tps;
    report.duration_secs = args.duration_secs;
    print_final_report(report);
    ui::success(&format!(
        "load test complete ({})",
        ui::format_elapsed(run_start)
    ));

    // Write full JSON report to a timestamped file so failures can be inspected afterwards.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let log_dir = std::path::Path::new("axe-load-test-logs");
    let log_path = log_dir.join(format!("axe-load-test-{ts}.json"));
    match std::fs::create_dir_all(log_dir) {
        Ok(()) => match serde_json::to_string_pretty(report) {
            Ok(json) => match std::fs::write(&log_path, &json) {
                Ok(()) => ui::info(&format!("report written to {}", log_path.display())),
                Err(e) => ui::warn(&format!(
                    "could not write report to {}: {e}",
                    log_path.display()
                )),
            },
            Err(e) => ui::warn(&format!("could not serialize report: {e}")),
        },
        Err(e) => ui::warn(&format!(
            "could not create log dir {}: {e}",
            log_dir.display()
        )),
    }

    Ok(())
}

/// List chain names that have a Cosmos Gateway address in the config.
pub(super) fn list_gateway_chains(config_root: &serde_json::Value) -> Vec<String> {
    config_root
        .pointer("/axelar/contracts/Gateway")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter(|(_, v)| v.get("address").and_then(|a| a.as_str()).is_some())
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Validate that an RPC endpoint speaks EVM JSON-RPC (eth_chainId).
pub(super) async fn validate_evm_rpc(rpc_url: &str) -> Result<()> {
    let provider = ProviderBuilder::new().connect_http(
        rpc_url
            .parse()
            .map_err(|e| eyre::eyre!("invalid RPC URL '{rpc_url}': {e}"))?,
    );
    provider.get_chain_id().await.map_err(|_| {
        eyre::eyre!(
            "RPC '{rpc_url}' does not appear to be an EVM endpoint \
             (eth_chainId failed). Check that you're using the correct RPC URL."
        )
    })?;
    Ok(())
}

/// Validate that an RPC endpoint speaks Solana JSON-RPC (getVersion).
pub(super) async fn validate_solana_rpc(rpc_url: &str) -> Result<()> {
    let client = solana_client::nonblocking::rpc_client::RpcClient::new(rpc_url.to_string());
    client.get_version().await.map_err(|_| {
        eyre::eyre!(
            "RPC '{rpc_url}' does not appear to be a Solana endpoint \
             (getVersion failed). Check that you're using the correct RPC URL."
        )
    })?;
    Ok(())
}

pub async fn check_evm_balance<P: Provider>(provider: &P, address: Address) -> Result<()> {
    let balance = provider.get_balance(address).await?;
    if balance.is_zero() {
        eyre::bail!(
            "EVM wallet {address} has no funds. Fund it first:\n  \
             Use a faucet or transfer native tokens to {address}"
        );
    }
    Ok(())
}

pub(super) async fn deploy_sender_receiver<P: Provider>(
    provider: &P,
    gateway: Address,
    gas_service: Address,
) -> Result<Address> {
    let bytecode = read_artifact_bytecode("artifacts/SenderReceiver.json")?;
    let mut deploy_code = bytecode;
    deploy_code.extend_from_slice(&(gateway, gas_service).abi_encode_params());

    let tx = TransactionRequest::default().with_deploy_code(Bytes::from(deploy_code));
    let pending = provider.send_transaction(tx).await?;
    let receipt = broadcast_and_log(pending, "deploy tx").await?;
    receipt
        .contract_address
        .ok_or_else(|| eyre::eyre!("no contract address in receipt"))
}

#[allow(clippy::float_arithmetic)]
fn print_final_report(report: &LoadTestReport) {
    println!();
    println!(
        "\u{2550}\u{2550}\u{2550} SUMMARY \u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}"
    );
    // Protocol + mode line
    {
        let mut mode_parts = vec![report.protocol.to_uppercase()];
        if let (Some(tps), Some(dur)) = (report.tps, report.duration_secs) {
            mode_parts.push(format!("{tps} tx/s"));
            mode_parts.push(format!("{dur}s"));
        }
        println!(
            "  {} -> {}  ({})",
            report.source_chain,
            report.destination_chain,
            mode_parts.join(", "),
        );
    }
    println!(
        "  transactions     {}/{} confirmed ({:.1}% landed)",
        report.total_confirmed,
        report.total_submitted,
        report.landing_rate * 100.0,
    );

    if let Some(ref v) = report.verification {
        println!();

        // Per-stage pipeline counts (computed from transaction records).
        // Shown even when verification times out so partial progress is visible.
        let total = report.total_confirmed;
        let voted = report
            .transactions
            .iter()
            .filter(|t| {
                t.amplifier_timing
                    .as_ref()
                    .is_some_and(|a| a.voted_secs.is_some())
            })
            .count() as u64;
        let routed = report
            .transactions
            .iter()
            .filter(|t| {
                t.amplifier_timing
                    .as_ref()
                    .is_some_and(|a| a.routed_secs.is_some())
            })
            .count() as u64;
        let approved = report
            .transactions
            .iter()
            .filter(|t| {
                t.amplifier_timing
                    .as_ref()
                    .is_some_and(|a| a.approved_secs.is_some())
            })
            .count() as u64;
        let executed = report
            .transactions
            .iter()
            .filter(|t| {
                t.amplifier_timing
                    .as_ref()
                    .is_some_and(|a| a.executed_secs.is_some())
            })
            .count() as u64;
        {
            let mut parts = Vec::new();
            // Skip voted for consensus chains (no VotingVerifier → always 0).
            if voted > 0 {
                parts.push(format!("voted {voted}/{total}"));
            }
            parts.push(format!("routed {routed}/{total}"));
            parts.push(format!("approved {approved}/{total}"));
            parts.push(format!("executed {executed}/{total}"));
            println!("  pipeline         {}", parts.join("  "));
        }

        // End-to-end line
        match (
            v.avg_executed_secs,
            v.min_executed_secs,
            v.max_executed_secs,
        ) {
            (Some(avg), Some(min), Some(max)) => {
                println!(
                    "  end-to-end       avg {avg:.1}s \u{2502} min {min:.1}s \u{2502} max {max:.1}s"
                );
            }
            (Some(avg), _, Some(max)) => {
                println!("  end-to-end       avg {avg:.1}s \u{2502} max {max:.1}s");
            }
            (Some(avg), _, _) => {
                println!("  end-to-end       avg {avg:.1}s");
            }
            _ => {}
        }

        // Sustained throughput per pipeline step (tx/s over each step's full span).
        {
            let p = &v.peak_throughput;
            let mut parts = Vec::new();
            if let Some(t) = p.voted_tps {
                parts.push(("voted", t));
            }
            if let Some(t) = p.routed_tps {
                parts.push(("routed", t));
            }
            if let Some(t) = p.hub_approved_tps {
                parts.push(("hub approved", t));
            }
            if let Some(t) = p.approved_tps {
                parts.push(("approved", t));
            }
            if let Some(t) = p.executed_tps {
                parts.push(("executed", t));
            }
            if !parts.is_empty() {
                println!("  throughput (sustained, tx/s)");
                for (name, rate) in &parts {
                    println!("    {name:<14} {rate:.1}");
                }
            }
        }

        // Latency percentiles (end-to-end, send → executed).
        {
            let mut latencies: Vec<f64> = report
                .transactions
                .iter()
                .filter_map(|t| t.amplifier_timing.as_ref()?.executed_secs)
                .collect();
            latencies.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let n = latencies.len();
            if n > 0 {
                let pct = |p: f64| -> f64 {
                    let idx = ((n as f64 * p) as usize).min(n - 1);
                    latencies[idx]
                };
                println!(
                    "  latency          p50 {:.1}s │ p90 {:.1}s │ p99 {:.1}s",
                    pct(0.50),
                    pct(0.90),
                    pct(0.99),
                );
            }
        }

        // Segment breakdown — show step duration + cumulative total.
        // Each avg_*_secs value is cumulative from tx send. Step = current - previous.
        // Steps are sorted by cumulative time so ITS hub_approved (which can precede
        // routed) displays in the correct chronological order.
        let src = &report.source_chain;
        let dst = &report.destination_chain;

        // Collect all available steps with their cumulative totals and labels.
        let mut steps: Vec<(f64, &str, String)> = Vec::new();
        if let Some(total) = report.avg_latency_ms.map(|ms| ms / 1000.0) {
            steps.push((total, "confirm", format!("({src})")));
        }
        if let Some(total) = v.avg_voted_secs {
            steps.push((total, "voted", "(axelar)".to_string()));
        }
        if let Some(total) = v.avg_routed_secs {
            steps.push((total, "routed", "(axelar)".to_string()));
        }
        if let Some(total) = v.avg_hub_approved_secs {
            steps.push((total, "hub approved", "(axelar hub)".to_string()));
        }
        if let Some(total) = v.avg_approved_secs {
            steps.push((total, "approved", format!("({dst})")));
        }
        if let Some(total) = v.avg_executed_secs {
            steps.push((total, "executed", format!("({dst})")));
        }

        // Sort by cumulative time so steps display in chronological order.
        steps.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        let mut prev: Option<f64> = None;
        for (i, (total, name, location)) in steps.iter().enumerate() {
            let step = prev.map_or(*total, |p| total - p);
            let is_last = i == steps.len() - 1;
            let connector = if is_last {
                "\u{2514}\u{2500}"
            } else {
                "\u{251c}\u{2500}"
            };
            println!(
                "  {} step {step:.1}s \u{2502} total {total:.1}s  {}",
                format!("{connector} {name:<13}").dimmed(),
                location.dimmed(),
            );
            prev = Some(*total);
        }

        // Stuck
        if v.stuck > 0 {
            let stuck_detail: Vec<String> = v
                .stuck_at
                .iter()
                .map(|c| format!("{} at {}", c.count, c.reason))
                .collect();
            println!();
            println!(
                "  stuck            {}/{} ({:.1}%) \u{2014} {}",
                v.stuck,
                v.total_verified,
                if v.total_verified > 0 {
                    v.stuck as f64 / v.total_verified as f64 * 100.0
                } else {
                    0.0
                },
                stuck_detail.join(", "),
            );
        }

        // Failures
        println!("  failures         {}", v.failed - v.stuck,);
        for cat in &v.failure_reasons {
            if !cat.reason.contains("timed out") {
                println!("                   {} \u{00d7} {}", cat.count, cat.reason);
            }
        }
    }
    println!();
}
