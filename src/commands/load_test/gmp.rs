//! Per-pair GMP load-test orchestrators. Each function:
//!
//! 1. Validates RPCs and the verification-contract config.
//! 2. Sets up signers, providers, and (re-)deploys SenderReceiver where needed.
//! 3. Runs the appropriate sender (`sol_sender` or `evm_sender`) for the
//!    chosen mode (one-shot vs sustained), optionally streaming verification
//!    in parallel for sustained sol-source flows.
//! 4. Hands off to `verify::*` and finalises the report.

use std::time::Instant;

use alloy::{
    primitives::Address,
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
};
use eyre::Result;
use serde_json::json;

use super::helpers::{
    check_evm_balance, deploy_or_reuse_sender_receiver, deploy_sender_receiver, finish_report,
    list_gateway_chains, validate_evm_rpc, validate_solana_rpc,
};
use super::resolve::{read_cache, save_cache};
use super::{LoadTestArgs, evm_sender, sol_sender, verify};
use crate::cosmos::read_axelar_contract_field;
use crate::ui;
use crate::utils::read_contract_address;

pub(super) async fn run_sol_to_evm(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let dest = &args.destination_chain;
    let src = &args.source_chain;

    let config_content = std::fs::read_to_string(&args.config)
        .map_err(|e| eyre::eyre!("failed to read config {}: {e}", args.config.display()))?;
    let config_root: serde_json::Value = serde_json::from_str(&config_content)?;

    let rpc_url = &args.destination_rpc;

    // Validate RPCs before doing any work
    validate_solana_rpc(&args.source_rpc).await?;
    validate_evm_rpc(rpc_url).await?;

    // Check that verification contracts exist for this chain pair before doing any work
    if read_axelar_contract_field(
        &args.config,
        &format!("/axelar/contracts/Gateway/{dest}/address"),
    )
    .is_err()
    {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — \
             verification would fail. Pick a chain that has a Gateway entry, e.g.:\n  {}",
            list_gateway_chains(&config_root).join(", ")
        );
    }

    ui::kv("source", src);
    ui::kv("destination", dest);

    let gateway_addr = read_contract_address(&args.config, dest, "AxelarGateway")?;
    let gas_service_addr = read_contract_address(&args.config, dest, "AxelarGasService")?;

    ui::address("EVM gateway", &format!("{gateway_addr}"));

    // --- Deploy/reuse SenderReceiver on destination EVM chain ---
    let cache = read_cache(dest);

    let (sender_receiver_addr, provider) = if let Some(addr_str) =
        cache.get("senderReceiverAddress").and_then(|v| v.as_str())
    {
        // Try to reuse cached address — only need a read-only provider for the check
        let read_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
        let addr: Address = addr_str.parse()?;
        let code = read_provider.get_code_at(addr).await?;
        // Check if code exists and gateway matches config
        let needs_redeploy = if code.is_empty() {
            ui::warn("cached SenderReceiver has no code, redeploying...");
            true
        } else {
            let sr = crate::evm::SenderReceiver::new(addr, &read_provider);
            match sr.gateway().call().await {
                Ok(onchain_gw) if onchain_gw != gateway_addr => {
                    ui::warn(&format!(
                        "cached SenderReceiver points to old gateway {onchain_gw}, expected {gateway_addr}, redeploying..."
                    ));
                    true
                }
                Err(_) => {
                    ui::warn("cached SenderReceiver gateway check failed, redeploying...");
                    true
                }
                _ => false,
            }
        };
        if needs_redeploy {
            let private_key = args.private_key.as_ref().ok_or_else(|| {
                eyre::eyre!("EVM private key required to deploy SenderReceiver. Set EVM_PRIVATE_KEY env var or use --private-key")
            })?;
            let signer: PrivateKeySigner = private_key.parse()?;
            check_evm_balance(&read_provider, signer.address()).await?;
            let write_provider = ProviderBuilder::new()
                .wallet(signer)
                .connect_http(rpc_url.parse()?);
            let new_addr =
                deploy_sender_receiver(&write_provider, gateway_addr, gas_service_addr).await?;
            let mut cache = cache;
            cache["senderReceiverAddress"] = json!(format!("{new_addr}"));
            save_cache(dest, &cache)?;
            (new_addr, write_provider)
        } else {
            ui::info(&format!("SenderReceiver: reusing {addr}"));
            let private_key = args
                .private_key
                .as_deref()
                .unwrap_or("0x0000000000000000000000000000000000000000000000000000000000000001");
            let signer: PrivateKeySigner = private_key.parse()?;
            let provider = ProviderBuilder::new()
                .wallet(signer)
                .connect_http(rpc_url.parse()?);
            (addr, provider)
        }
    } else {
        let private_key = args.private_key.as_ref().ok_or_else(|| {
            eyre::eyre!("EVM private key required to deploy SenderReceiver. Set EVM_PRIVATE_KEY env var or use --private-key")
        })?;
        let signer: PrivateKeySigner = private_key.parse()?;
        let deployer_addr = signer.address();
        let read_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
        check_evm_balance(&read_provider, deployer_addr).await?;
        ui::info("deploying SenderReceiver on destination chain...");
        let write_provider = ProviderBuilder::new()
            .wallet(signer)
            .connect_http(rpc_url.parse()?);
        let addr = deploy_sender_receiver(&write_provider, gateway_addr, gas_service_addr).await?;
        let mut cache = cache;
        cache["senderReceiverAddress"] = json!(format!("{addr}"));
        save_cache(dest, &cache)?;
        (addr, write_provider)
    };

    ui::address("SenderReceiver", &format!("{sender_receiver_addr}"));
    let destination_address = format!("{sender_receiver_addr}");

    let test_start = Instant::now();
    let mut report = if args.tps.is_some() && args.duration_secs.is_some() {
        {
            let (spinner_tx, _spinner_rx) =
                tokio::sync::oneshot::channel::<indicatif::ProgressBar>();
            sol_sender::run_sustained_load_test_with_metrics(
                &args,
                true,
                &destination_address,
                None,
                None,
                spinner_tx,
            )
            .await?
        }
    } else {
        sol_sender::run_load_test_with_metrics(&args, &destination_address, true).await?
    };

    let verification = verify::verify_onchain(
        &args.config,
        &args.source_axelar_id,
        &args.destination_axelar_id,
        &destination_address,
        gateway_addr,
        &provider,
        &mut report.transactions,
        verify::SourceChainType::Svm,
    )
    .await?;
    report.verification = Some(verification);

    finish_report(&args, &mut report, test_start)
}

pub(super) async fn run_evm_to_sol(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let config_content = std::fs::read_to_string(&args.config)
        .map_err(|e| eyre::eyre!("failed to read config {}: {e}", args.config.display()))?;
    let config_root: serde_json::Value = serde_json::from_str(&config_content)?;

    let evm_rpc_url = args.source_rpc.clone();

    // Validate RPCs before doing any work
    validate_evm_rpc(&evm_rpc_url).await?;
    validate_solana_rpc(&args.destination_rpc).await?;

    // Check that verification contracts exist for this chain pair before doing any work
    if read_axelar_contract_field(
        &args.config,
        &format!("/axelar/contracts/Gateway/{dest}/address"),
    )
    .is_err()
    {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — \
             verification would fail. Pick a chain that has a Gateway entry, e.g.:\n  {}",
            list_gateway_chains(&config_root).join(", ")
        );
    }

    ui::kv("source", src);
    ui::kv("destination", dest);

    let gateway_addr = read_contract_address(&args.config, src, "AxelarGateway")?;
    let gas_service_addr =
        read_contract_address(&args.config, src, "AxelarGasService").unwrap_or(Address::ZERO);
    ui::address("EVM gateway", &format!("{gateway_addr}"));

    // --- Set up EVM signer ---
    let private_key = args.private_key.as_ref().ok_or_else(|| {
        eyre::eyre!("EVM private key required. Set EVM_PRIVATE_KEY env var or use --private-key")
    })?;
    let signer: PrivateKeySigner = private_key.parse()?;
    let signer_address = signer.address();
    let read_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    check_evm_balance(&read_provider, signer_address).await?;

    let write_provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(evm_rpc_url.parse()?);

    // Extract 32-byte private key for deriving sub-wallets
    let main_key: [u8; 32] = signer.to_bytes().into();

    #[allow(clippy::float_arithmetic)]
    {
        let balance: u128 = read_provider.get_balance(signer_address).await?.to();
        let eth = balance as f64 / 1e18;
        ui::kv("wallet", &format!("{signer_address} ({eth:.6} ETH)"));
    }

    // --- Deploy/reuse SenderReceiver on source chain ---
    let cache_key = &format!("{src}-evm-to-sol");
    let cache = read_cache(cache_key);
    let sender_receiver_addr = deploy_or_reuse_sender_receiver(
        &cache,
        cache_key,
        &read_provider,
        &write_provider,
        gateway_addr,
        gas_service_addr,
        "source",
    )
    .await?;
    ui::address("SenderReceiver", &format!("{sender_receiver_addr}"));

    // Destination on Solana: memo program (resolved per feature flag)
    let destination_address = evm_sender::memo_program_id().to_string();
    let destination_address = destination_address.as_str();
    ui::kv("destination program", destination_address);

    let test_start = Instant::now();
    let sustained = args.tps.is_some() && args.duration_secs.is_some();

    let mut report = if sustained {
        // Sustained mode: run verification concurrently with the send phase.
        let (verify_tx, verify_rx) = tokio::sync::mpsc::unbounded_channel();
        let send_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // The MultiProgress + spinners are created inside the send function AFTER
        // funding completes, so they don't flicker during setup. The verify spinner
        // is sent to the verify task via a oneshot channel.
        let (spinner_tx, spinner_rx) = tokio::sync::oneshot::channel::<indicatif::ProgressBar>();

        // Spawn verification in a background task.
        let vconfig = args.config.clone();
        let vsource = args.source_axelar_id.clone();
        let vdest = args.destination_axelar_id.clone();
        let vdest_addr = destination_address.to_string();
        let vdest_rpc = args.destination_rpc.clone();
        let vdone = std::sync::Arc::clone(&send_done);
        let verify_handle = tokio::spawn(async move {
            let spinner = spinner_rx.await.expect("spinner channel dropped");
            verify::verify_onchain_solana_streaming(
                &vconfig,
                &vsource,
                &vdest,
                &vdest_addr,
                &vdest_rpc,
                verify_rx,
                vdone,
                spinner,
            )
            .await
        });

        let mut report = evm_sender::run_sustained_load_test_with_metrics(
            &args,
            sender_receiver_addr,
            &main_key,
            &evm_rpc_url,
            destination_address,
            Some(verify_tx),
            Some(send_done),
            spinner_tx,
            false,
        )
        .await?;

        // Wait for verification to finish.
        let (verification, timings) = verify_handle.await??;
        // Write amplifier timing back into per-tx records for JSON report & pipeline counts.
        // Timings are keyed by message_id (signature); match them to transactions.
        for (msg_id, timing) in timings {
            if let Some(tx) = report.transactions.iter_mut().find(|t| {
                t.signature == msg_id
                    || format!(
                        "{}-{}.1",
                        t.signature,
                        crate::solana::solana_call_contract_index()
                    ) == msg_id
            }) {
                tx.amplifier_timing = Some(timing);
            }
        }
        report.verification = Some(verification);
        report
    } else {
        let mut report = evm_sender::run_load_test_with_metrics(
            &args,
            sender_receiver_addr,
            &main_key,
            &evm_rpc_url,
            destination_address,
            false,
        )
        .await?;

        let verification = verify::verify_onchain_solana(
            &args.config,
            &args.source_axelar_id,
            &args.destination_axelar_id,
            destination_address,
            &args.destination_rpc,
            &mut report.transactions,
            verify::SourceChainType::Evm,
        )
        .await?;
        report.verification = Some(verification);
        report
    };

    finish_report(&args, &mut report, test_start)
}

pub(super) async fn run_evm_to_evm(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let config_content = std::fs::read_to_string(&args.config)
        .map_err(|e| eyre::eyre!("failed to read config {}: {e}", args.config.display()))?;
    let config_root: serde_json::Value = serde_json::from_str(&config_content)?;

    let source_rpc_url = args.source_rpc.clone();
    let dest_rpc_url = args.destination_rpc.clone();

    // Validate RPCs before doing any work
    validate_evm_rpc(&source_rpc_url).await?;
    validate_evm_rpc(&dest_rpc_url).await?;

    // Check that verification contracts exist
    if read_axelar_contract_field(
        &args.config,
        &format!("/axelar/contracts/Gateway/{dest}/address"),
    )
    .is_err()
    {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — \
             verification would fail. Pick a chain that has a Gateway entry, e.g.:\n  {}",
            list_gateway_chains(&config_root).join(", ")
        );
    }

    ui::kv("source", src);
    ui::kv("destination", dest);

    // --- Set up EVM signer ---
    let private_key = args.private_key.as_ref().ok_or_else(|| {
        eyre::eyre!("EVM private key required. Set EVM_PRIVATE_KEY env var or use --private-key")
    })?;
    let signer: PrivateKeySigner = private_key.parse()?;
    let signer_address = signer.address();
    let source_read_provider = ProviderBuilder::new().connect_http(source_rpc_url.parse()?);
    check_evm_balance(&source_read_provider, signer_address).await?;

    let source_write_provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(source_rpc_url.parse()?);

    let main_key: [u8; 32] = signer.to_bytes().into();

    #[allow(clippy::float_arithmetic)]
    {
        let balance: u128 = source_read_provider.get_balance(signer_address).await?.to();
        let eth = balance as f64 / 1e18;
        ui::kv("wallet", &format!("{signer_address} ({eth:.6} ETH)"));
    }

    // --- Source chain: deploy/reuse SenderReceiver (for sending) ---
    let src_gateway_addr = read_contract_address(&args.config, src, "AxelarGateway")?;
    let src_gas_service_addr =
        read_contract_address(&args.config, src, "AxelarGasService").unwrap_or(Address::ZERO);
    ui::address("source gateway", &format!("{src_gateway_addr}"));

    let src_cache_key = &format!("{src}-evm-to-evm");
    let src_cache = read_cache(src_cache_key);
    let sender_receiver_addr = deploy_or_reuse_sender_receiver(
        &src_cache,
        src_cache_key,
        &source_read_provider,
        &source_write_provider,
        src_gateway_addr,
        src_gas_service_addr,
        "source",
    )
    .await?;
    ui::address(
        "SenderReceiver (source)",
        &format!("{sender_receiver_addr}"),
    );

    // --- Destination chain: deploy/reuse SenderReceiver (as receive target) ---
    let dest_gateway_addr = read_contract_address(&args.config, dest, "AxelarGateway")?;
    let dest_gas_service_addr =
        read_contract_address(&args.config, dest, "AxelarGasService").unwrap_or(Address::ZERO);
    ui::address("destination gateway", &format!("{dest_gateway_addr}"));

    let dest_read_provider = ProviderBuilder::new().connect_http(dest_rpc_url.parse()?);
    let dest_write_provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(dest_rpc_url.parse()?);

    // Fund the signer on the destination chain if needed for SenderReceiver deployment
    let dest_balance: u128 = dest_read_provider.get_balance(signer_address).await?.to();
    if dest_balance == 0 {
        eyre::bail!(
            "EVM wallet {signer_address} has no funds on destination chain '{dest}'. \
             Fund it first."
        );
    }

    let dest_cache_key = &format!("{dest}-evm-to-evm-dest");
    let dest_cache = read_cache(dest_cache_key);
    let dest_sender_receiver = deploy_or_reuse_sender_receiver(
        &dest_cache,
        dest_cache_key,
        &dest_read_provider,
        &dest_write_provider,
        dest_gateway_addr,
        dest_gas_service_addr,
        "destination",
    )
    .await?;
    ui::address(
        "SenderReceiver (destination)",
        &format!("{dest_sender_receiver}"),
    );

    let destination_address = format!("{dest_sender_receiver}");

    let test_start = Instant::now();
    let mut report = if args.tps.is_some() && args.duration_secs.is_some() {
        evm_sender::run_sustained_load_test_with_metrics(
            &args,
            sender_receiver_addr,
            &main_key,
            &source_rpc_url,
            &destination_address,
            None,
            None,
            tokio::sync::oneshot::channel().0,
            true,
        )
        .await?
    } else {
        evm_sender::run_load_test_with_metrics(
            &args,
            sender_receiver_addr,
            &main_key,
            &source_rpc_url,
            &destination_address,
            true,
        )
        .await?
    };

    let verification = verify::verify_onchain(
        &args.config,
        &args.source_axelar_id,
        &args.destination_axelar_id,
        &destination_address,
        dest_gateway_addr,
        &dest_read_provider,
        &mut report.transactions,
        verify::SourceChainType::Evm,
    )
    .await?;
    report.verification = Some(verification);

    finish_report(&args, &mut report, test_start)
}

pub(super) async fn run_sol_to_sol(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    // Validate RPCs
    validate_solana_rpc(&args.source_rpc).await?;
    validate_solana_rpc(&args.destination_rpc).await?;

    let config_content = std::fs::read_to_string(&args.config)
        .map_err(|e| eyre::eyre!("failed to read config {}: {e}", args.config.display()))?;
    let config_root: serde_json::Value = serde_json::from_str(&config_content)?;

    // Check that verification contracts exist
    if read_axelar_contract_field(
        &args.config,
        &format!("/axelar/contracts/Gateway/{dest}/address"),
    )
    .is_err()
    {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — \
             verification would fail. Pick a chain that has a Gateway entry, e.g.:\n  {}",
            list_gateway_chains(&config_root).join(", ")
        );
    }

    ui::kv("source", src);
    ui::kv("destination", dest);

    // Destination is the Solana memo program
    let destination_address = evm_sender::memo_program_id().to_string();
    let destination_address = destination_address.as_str();
    ui::kv("destination program", destination_address);

    let test_start = Instant::now();
    let sustained = args.tps.is_some() && args.duration_secs.is_some();

    let mut report = if sustained {
        // Sustained mode: run verification concurrently with the send phase.
        let (verify_tx, verify_rx) = tokio::sync::mpsc::unbounded_channel();
        let send_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (spinner_tx, spinner_rx) = tokio::sync::oneshot::channel::<indicatif::ProgressBar>();

        // Spawn verification in a background task.
        let vconfig = args.config.clone();
        let vsource = args.source_axelar_id.clone();
        let vdest = args.destination_axelar_id.clone();
        let vdest_addr = destination_address.to_string();
        let vdest_rpc = args.destination_rpc.clone();
        let vdone = std::sync::Arc::clone(&send_done);
        let verify_handle = tokio::spawn(async move {
            let spinner = spinner_rx.await.expect("spinner channel dropped");
            verify::verify_onchain_solana_streaming(
                &vconfig,
                &vsource,
                &vdest,
                &vdest_addr,
                &vdest_rpc,
                verify_rx,
                vdone,
                spinner,
            )
            .await
        });

        let mut report = sol_sender::run_sustained_load_test_with_metrics(
            &args,
            false,
            destination_address,
            Some(verify_tx),
            Some(send_done),
            spinner_tx,
        )
        .await?;

        // Wait for verification to finish.
        let (verification, timings) = verify_handle.await??;
        for (msg_id, timing) in timings {
            if let Some(tx) = report.transactions.iter_mut().find(|t| {
                t.signature == msg_id
                    || format!(
                        "{}-{}.1",
                        t.signature,
                        crate::solana::solana_call_contract_index()
                    ) == msg_id
            }) {
                tx.amplifier_timing = Some(timing);
            }
        }
        report.verification = Some(verification);
        report
    } else {
        let mut report =
            sol_sender::run_load_test_with_metrics(&args, destination_address, false).await?;

        let verification = verify::verify_onchain_solana(
            &args.config,
            &args.source_axelar_id,
            &args.destination_axelar_id,
            destination_address,
            &args.destination_rpc,
            &mut report.transactions,
            verify::SourceChainType::Svm,
        )
        .await?;
        report.verification = Some(verification);
        report
    };

    finish_report(&args, &mut report, test_start)
}
