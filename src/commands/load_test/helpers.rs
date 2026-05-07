//! Shared helpers used by every per-pair load-test orchestrator:
//! SenderReceiver deploy/reuse, RPC validation, the report finalizer, and the
//! per-chain config readers (Stellar/Sui wallet loaders, JSON-pointer
//! contract-address lookups). Most of this module is `pub(super)` — only
//! `ensure_sender_receiver_on_evm_chain` is `pub(crate)` because
//! `commands::test_gmp` calls into it for the `--config` sol→evm flow.

use std::time::Instant;

use alloy::{
    network::TransactionBuilder,
    primitives::{Address, Bytes},
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
    sol_types::SolValue,
};
use eyre::Result;
use owo_colors::OwoColorize;
use serde_json::json;

use super::metrics::LoadTestReport;
use super::verify;
use super::{LoadTestArgs, read_cache, save_cache};
use crate::config::ChainsConfig;
use crate::evm::read_artifact_bytecode;
use crate::ui;

pub(crate) async fn ensure_sender_receiver_on_evm_chain(
    chain: &str,
    rpc_url: &str,
    evm_private_key: &str,
    gateway_addr: Address,
    gas_service_addr: Address,
) -> Result<Address> {
    use alloy::signers::local::PrivateKeySigner;
    let signer: PrivateKeySigner = evm_private_key.parse()?;
    let read_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let write_provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(rpc_url.parse()?);
    let cache = read_cache(chain);
    deploy_or_reuse_sender_receiver(
        &cache,
        chain,
        &read_provider,
        &write_provider,
        gateway_addr,
        gas_service_addr,
        chain,
    )
    .await
}

/// Deploy or reuse a cached SenderReceiver contract.
pub(crate) async fn deploy_or_reuse_sender_receiver<R: Provider, W: Provider>(
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

pub(crate) fn finish_report(
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
/// Used by the load-test runners to print a remediation hint when the user
/// supplies a destination chain whose Gateway has not been deployed yet.
pub(crate) fn list_gateway_chains(cfg: &ChainsConfig) -> Vec<String> {
    cfg.axelar
        .contracts
        .as_ref()
        .and_then(|c| c.get("Gateway"))
        .map(|gateway_map| {
            gateway_map
                .iter()
                .filter(|(_, v)| v.get("address").and_then(|a| a.as_str()).is_some())
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Validate that an RPC endpoint speaks EVM JSON-RPC (eth_chainId).
pub(crate) async fn validate_evm_rpc(rpc_url: &str) -> Result<()> {
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

/// Pre-flight check that a contract address actually has bytecode at the
/// given EVM RPC. Without this, the EVM destination verifier silently reports
/// false-positive 30/30 executed — `eth_call` against an EOA returns `0x`,
/// which alloy decodes as `false`, which our pipeline interprets as
/// "approval consumed by execution = success." See verify.rs:266 for the
/// dependent decode logic.
pub(crate) async fn ensure_evm_contract_deployed(
    rpc_url: &str,
    contract_label: &str,
    addr: alloy::primitives::Address,
) -> Result<()> {
    let provider =
        ProviderBuilder::new().connect_http(rpc_url.parse().map_err(|e| eyre::eyre!("{e}"))?);
    let code = provider
        .get_code_at(addr)
        .await
        .map_err(|e| eyre::eyre!("eth_getCode for {contract_label} ({addr}) failed: {e}"))?;
    if code.is_empty() {
        eyre::bail!(
            "{contract_label} at {addr} on {rpc_url} has no bytecode. \
             The chain config likely points at an undeployed/stale address — \
             this environment cannot relay messages to that contract. \
             Pick a different chain pair or update the chain config to a deployed address."
        );
    }
    Ok(())
}

/// Validate that an RPC endpoint speaks Solana JSON-RPC (getVersion).
pub(crate) async fn validate_solana_rpc(rpc_url: &str) -> Result<()> {
    let client = solana_client::nonblocking::rpc_client::RpcClient::new(rpc_url.to_string());
    client.get_version().await.map_err(|_| {
        eyre::eyre!(
            "RPC '{rpc_url}' does not appear to be a Solana endpoint \
             (getVersion failed). Check that you're using the correct RPC URL."
        )
    })?;
    Ok(())
}

pub(crate) async fn check_evm_balance<P: alloy::providers::Provider>(
    provider: &P,
    address: alloy::primitives::Address,
) -> Result<()> {
    let balance = provider.get_balance(address).await?;
    if balance.is_zero() {
        eyre::bail!(
            "EVM wallet {address} has no funds. Fund it first:\n  \
             Use a faucet or transfer native tokens to {address}"
        );
    }
    Ok(())
}

pub(crate) async fn deploy_sender_receiver<P: alloy::providers::Provider>(
    provider: &P,
    gateway: alloy::primitives::Address,
    gas_service: alloy::primitives::Address,
) -> Result<alloy::primitives::Address> {
    let bytecode = read_artifact_bytecode("artifacts/SenderReceiver.json")?;
    let mut deploy_code = bytecode;
    deploy_code.extend_from_slice(&(gateway, gas_service).abi_encode_params());

    let tx = TransactionRequest::default().with_deploy_code(Bytes::from(deploy_code));
    let pending = provider.send_transaction(tx).await?;
    let tx_hash = *pending.tx_hash();
    ui::tx_hash("deploy tx", &format!("{tx_hash}"));
    ui::info("waiting for confirmation...");

    let receipt = tokio::time::timeout(std::time::Duration::from_secs(120), pending.get_receipt())
        .await
        .map_err(|_| eyre::eyre!("deploy tx timed out after 120s"))??;

    let addr = receipt
        .contract_address
        .ok_or_else(|| eyre::eyre!("no contract address in receipt"))?;

    ui::success(&format!(
        "deployed in block {}",
        receipt.block_number.unwrap_or(0)
    ));
    Ok(addr)
}

#[allow(clippy::float_arithmetic)]
pub(crate) fn print_final_report(report: &LoadTestReport) {
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
pub(crate) fn axelar_id_for_chain(config: &std::path::Path, chain_id: &str) -> Result<String> {
    let content =
        std::fs::read_to_string(config).map_err(|e| eyre::eyre!("failed to read config: {e}"))?;
    let root: serde_json::Value = serde_json::from_str(&content)?;
    Ok(root
        .pointer(&format!("/chains/{chain_id}/axelarId"))
        .and_then(|v| v.as_str())
        .unwrap_or(chain_id)
        .to_string())
}

pub(crate) fn read_stellar_network_type(
    config: &std::path::Path,
    chain_id: &str,
) -> Result<String> {
    let content =
        std::fs::read_to_string(config).map_err(|e| eyre::eyre!("failed to read config: {e}"))?;
    let root: serde_json::Value = serde_json::from_str(&content)?;
    Ok(root
        .pointer(&format!("/chains/{chain_id}/networkType"))
        .and_then(|v| v.as_str())
        .unwrap_or("testnet")
        .to_string())
}

pub(crate) fn read_stellar_token_address(
    config: &std::path::Path,
    chain_id: &str,
) -> Result<String> {
    let content =
        std::fs::read_to_string(config).map_err(|e| eyre::eyre!("failed to read config: {e}"))?;
    let root: serde_json::Value = serde_json::from_str(&content)?;
    root.pointer(&format!("/chains/{chain_id}/tokenAddress"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| {
            eyre::eyre!("no tokenAddress (XLM Soroban contract) for Stellar chain {chain_id}")
        })
}

pub(crate) fn read_stellar_contract_address(
    config: &std::path::Path,
    chain_id: &str,
    contract: &str,
) -> Result<String> {
    let content =
        std::fs::read_to_string(config).map_err(|e| eyre::eyre!("failed to read config: {e}"))?;
    let root: serde_json::Value = serde_json::from_str(&content)?;
    root.pointer(&format!("/chains/{chain_id}/contracts/{contract}/address"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| eyre::eyre!("no Stellar contract {contract} for chain {chain_id}"))
}

pub(crate) fn load_stellar_main_wallet(
    private_key: Option<&str>,
) -> Result<crate::stellar::StellarWallet> {
    let key = private_key
        .map(String::from)
        .or_else(|| std::env::var("STELLAR_PRIVATE_KEY").ok())
        .ok_or_else(|| {
            eyre::eyre!(
                "Stellar main wallet required. Set STELLAR_PRIVATE_KEY to either an S... secret key \
                 or a 32-byte hex seed."
            )
        })?;
    if key.starts_with('S') && key.len() > 50 {
        crate::stellar::StellarWallet::from_secret_str(&key)
    } else {
        crate::stellar::StellarWallet::from_hex_seed(&key)
    }
}
pub(crate) async fn ensure_sender_receiver(
    args: &LoadTestArgs,
    rpc_url: &str,
    gateway_addr: Address,
    gas_service_addr: Address,
    cache: serde_json::Value,
    evm_private_key: Option<&str>,
) -> Result<(Address, impl Provider)> {
    if let Some(addr_str) = cache.get("senderReceiverAddress").and_then(|v| v.as_str()) {
        let read_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
        let addr: Address = addr_str.parse()?;
        let code = read_provider.get_code_at(addr).await?;
        let needs_redeploy = if code.is_empty() {
            true
        } else {
            let sr = crate::evm::SenderReceiver::new(addr, &read_provider);
            !matches!(sr.gateway().call().await, Ok(onchain_gw) if onchain_gw == gateway_addr)
        };
        if !needs_redeploy {
            // Wallet provider — caller may submit txs through it. Fail loud
            // when no key is configured rather than substitute the historic
            // `0x0…01` placeholder, which was a sweepable-funds footgun.
            let pk = args
                .private_key
                .as_deref()
                .or(evm_private_key)
                .ok_or_else(|| {
                    eyre::eyre!(
                        "EVM private key required to reuse the cached SenderReceiver. \
                     Set EVM_PRIVATE_KEY env var or use --private-key"
                    )
                })?;
            let signer: PrivateKeySigner = pk.parse()?;
            let provider = ProviderBuilder::new()
                .wallet(signer)
                .connect_http(rpc_url.parse()?);
            return Ok((addr, provider));
        }
    }

    let pk = args.private_key.as_deref().or(evm_private_key).ok_or_else(|| {
        eyre::eyre!(
            "EVM private key required to deploy SenderReceiver. Set EVM_PRIVATE_KEY env var or use --private-key"
        )
    })?;
    let signer: PrivateKeySigner = pk.parse()?;
    let write_provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(rpc_url.parse()?);
    let addr = deploy_sender_receiver(&write_provider, gateway_addr, gas_service_addr).await?;
    let mut cache = cache;
    cache["senderReceiverAddress"] = json!(format!("{addr}"));
    save_cache(&args.destination_chain, &cache)?;
    Ok((addr, write_provider))
}
pub(crate) fn load_sui_main_wallet() -> Result<crate::sui::SuiWallet> {
    let key = std::env::var("SUI_PRIVATE_KEY").map_err(|_| {
        eyre::eyre!(
            "SUI_PRIVATE_KEY required (a `suiprivkey1...` bech32 secret from `sui keytool` or 64-char hex). Add it to .env."
        )
    })?;
    crate::sui::SuiWallet::from_secret_str(&key)
}
pub(crate) fn sui_object_id(
    config: &std::path::Path,
    chain_id: &str,
    pointer_within_chain: &str,
) -> Result<String> {
    let content =
        std::fs::read_to_string(config).map_err(|e| eyre::eyre!("failed to read config: {e}"))?;
    let root: serde_json::Value = serde_json::from_str(&content)?;
    root.pointer(&format!("/chains/{chain_id}{pointer_within_chain}"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| eyre::eyre!("no Sui object {pointer_within_chain} for chain {chain_id}"))
}

/// Read `(sui_channel_id, sui_rpc)` from the chains config. `rpc_override`
/// lets the caller honor `--destination-rpc` / `DESTINATION_RPC` from
/// `LoadTestArgs::destination_rpc`. An empty/None override falls back to
/// the chain config's `rpc` field.
pub(crate) fn sui_dest_lookup(
    config: &std::path::Path,
    sui_chain_id: &str,
    rpc_override: Option<&str>,
) -> Result<(String, String)> {
    let channel = sui_object_id(
        config,
        sui_chain_id,
        "/contracts/Example/objects/GmpChannelId",
    )?;
    let (config_rpc, _contracts) = crate::sui::read_sui_chain_config(config, sui_chain_id)?;
    let rpc = match rpc_override {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => config_rpc,
    };
    Ok((channel, rpc))
}

/// Run the Sui destination verifier and stamp the report. Shared between
/// `run_evm_to_sui`, `run_sol_to_sui`, `run_stellar_to_sui`.
pub(crate) async fn finalize_sui_dest_run(
    args: &LoadTestArgs,
    report: &mut crate::commands::load_test::metrics::LoadTestReport,
    sui_channel: &str,
    sui_rpc: &str,
    source_type: verify::SourceChainType,
    test_start: Instant,
) -> Result<()> {
    let verification = verify::verify_onchain_sui_gmp(
        &args.config,
        &args.source_axelar_id,
        &args.destination_axelar_id,
        sui_channel,
        sui_rpc,
        &mut report.transactions,
        source_type,
    )
    .await?;
    report.verification = Some(verification);
    finish_report(args, report, test_start)
}
