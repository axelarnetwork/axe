//! XRPL -> any EVM ITS load test.
//!
//! Scope: transfers native XRP from XRPL to an EVM destination (today this is
//! XRPL-EVM, which already has the canonical XRP interchain token registered
//! against the Axelar gateway). The sender side is pure XRPL `Payment` with
//! the standard `interchain_transfer` memos; verification reuses the existing
//! EVM destination checker.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use eyre::{Result, eyre};

use super::{LoadTestArgs, finish_report, validate_evm_rpc, xrpl_sender};
use crate::cosmos::read_axelar_contract_field;
use crate::ui;
use crate::utils::read_contract_address;
use crate::xrpl::{
    XrplClient, XrplWallet, account_id_to_hex, faucet_url_for_network, parse_address,
};

pub async fn run(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    validate_evm_rpc(&args.destination_rpc).await?;

    // Verify Axelar-side prerequisites
    if read_axelar_contract_field(
        &args.config,
        &format!("/axelar/contracts/Gateway/{dest}/address"),
    )
    .is_err()
    {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — verification would fail."
        );
    }
    if read_axelar_contract_field(&args.config, "/axelar/contracts/AxelarnetGateway/address")
        .is_err()
    {
        eyre::bail!("no AxelarnetGateway address in config — required for ITS load test");
    }

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "ITS (XRP interchain_transfer via hub)");

    // --- XRPL config ---
    let (xrpl_rpc, xrpl_multisig_addr, xrpl_network_type) =
        read_xrpl_chain_config(&args.config, src)?;
    ui::address("XRPL multisig", &xrpl_multisig_addr);

    // --- XRPL main wallet ---
    let main_wallet = load_xrpl_main_wallet(args.private_key.as_deref())?;
    let xrpl_client = XrplClient::new(&xrpl_rpc);
    let main_info = xrpl_client.account_info(&main_wallet.address()).await?;
    let main_balance_drops = main_info.map(|i| i.balance_drops).unwrap_or(0);
    ui::kv(
        "XRPL wallet",
        &format!(
            "{} ({:.4} XRP)",
            main_wallet.address(),
            main_balance_drops as f64 / 1_000_000.0
        ),
    );

    // --- EVM destination address (ITS proxy) ---
    let its_proxy_addr = read_contract_address(&args.config, dest, "InterchainTokenService")?;
    ui::address("destination ITS", &format!("{its_proxy_addr}"));
    // For XRPL → EVM interchain_transfer, the destination_address memo carries
    // the hex-encoded destination bytes (the ITS proxy on the EVM side).
    let dest_address_hex = format!("{its_proxy_addr:x}")
        .trim_start_matches("0x")
        .to_string();

    let evm_gateway_addr = read_contract_address(&args.config, dest, "AxelarGateway")?;
    ui::address("EVM gateway", &format!("{evm_gateway_addr}"));

    let axelarnet_gw_addr =
        read_axelar_contract_field(&args.config, "/axelar/contracts/AxelarnetGateway/address")?;

    // --- Gas fee (XRP drops) ---
    let gas_fee_drops: u64 = match &args.gas_value {
        Some(v) => v.parse().map_err(|e| eyre!("invalid --gas-value: {e}"))?,
        None => xrpl_sender::DEFAULT_GAS_FEE_DROPS,
    };
    ui::kv(
        "gas fee",
        &format!(
            "{gas_fee_drops} drops ({:.4} XRP)",
            gas_fee_drops as f64 / 1_000_000.0
        ),
    );

    // --- Burst vs sustained ---
    let burst_mode = !(args.tps.is_some() && args.duration_secs.is_some());
    let (num_keys, _total_expected) = if burst_mode {
        let n = args.num_txs.max(1) as usize;
        (n, args.num_txs.max(1))
    } else {
        let tps = args.tps.unwrap() as usize;
        let dur = args.duration_secs.unwrap();
        (tps * args.key_cycle as usize, tps as u64 * dur)
    };

    // --- Fund ephemeral wallets ---
    let txs_per_key = if burst_mode {
        1u64
    } else {
        args.duration_secs.unwrap().div_ceil(args.key_cycle)
    };
    // Each wallet needs: base reserve (~10 XRP) + txs_per_key * (transfer + gas + base fee)
    let per_wallet_drops: u64 = 10_000_000u64
        + txs_per_key.saturating_mul(xrpl_sender::TRANSFER_AMOUNT_DROPS + gas_fee_drops + 100);

    let faucet_url = faucet_url_for_network(&xrpl_network_type);
    let main_seed = main_wallet.secret_key.serialize();
    let wallets = xrpl_sender::prepare_wallets(
        &xrpl_client,
        &main_seed,
        Some(&main_wallet),
        num_keys,
        per_wallet_drops,
        faucet_url,
    )
    .await?;

    let multisig = parse_address(&xrpl_multisig_addr)?;

    // --- Sustained mode ---
    if !burst_mode {
        let tps_n = args.tps.unwrap() as usize;
        let duration_secs = args.duration_secs.unwrap();
        let key_cycle = args.key_cycle as usize;

        let (verify_tx, verify_rx) = tokio::sync::mpsc::unbounded_channel();
        let send_done = Arc::new(AtomicBool::new(false));
        let (spinner_tx, spinner_rx) = tokio::sync::oneshot::channel::<indicatif::ProgressBar>();

        let vconfig = args.config.clone();
        let vsource = args.source_axelar_id.clone();
        let vdest = args.destination_axelar_id.clone();
        let vdest_rpc = args.destination_rpc.clone();
        let vdone = Arc::clone(&send_done);
        let vgw = evm_gateway_addr;
        let verify_handle = tokio::spawn(async move {
            let spinner = spinner_rx.await.expect("spinner channel dropped");
            super::verify::verify_onchain_evm_its_streaming(
                &vconfig, &vsource, &vdest, vgw, &vdest_rpc, verify_rx, vdone, spinner,
            )
            .await
        });

        let spinner = ui::wait_spinner(&format!(
            "[0/{duration_secs}s] starting sustained XRPL ITS send..."
        ));
        let _ = spinner_tx.send(spinner.clone());

        let test_start = Instant::now();

        // XRPL source has a separate VotingVerifier (XrplVotingVerifier). The
        // existing streaming verifier uses `VotingVerifier/{source}` — which
        // doesn't exist for XRPL — so we run without a voting check and let
        // the Routed → HubApproved → ... stages drive the pipeline.
        let has_voting_verifier = false;

        let result = xrpl_sender::run_sustained(
            &xrpl_client,
            wallets,
            multisig,
            dest.clone(),
            dest_address_hex.clone(),
            gas_fee_drops,
            "axelar".to_string(),
            axelarnet_gw_addr.clone(),
            tps_n,
            duration_secs,
            key_cycle,
            Some(verify_tx),
            Some(send_done),
            spinner,
            has_voting_verifier,
        )
        .await;

        let mut report = super::sustained::build_sustained_report(
            result,
            src,
            dest,
            &format!("{its_proxy_addr}"),
            tps_n as u64 * duration_secs,
            num_keys,
        );
        let (verification, timings) = verify_handle.await??;
        for (msg_id, timing) in timings {
            if let Some(tx) = report
                .transactions
                .iter_mut()
                .find(|t| t.signature == msg_id)
            {
                tx.amplifier_timing = Some(timing);
            }
        }
        report.verification = Some(verification);
        return finish_report(&args, &mut report, test_start);
    }

    // --- Burst mode ---
    let test_start = Instant::now();
    let mut report = xrpl_sender::run_burst(
        &xrpl_client,
        &wallets,
        &multisig,
        dest,
        &dest_address_hex,
        gas_fee_drops,
        "axelar",
        &axelarnet_gw_addr,
        src,
        dest,
    )
    .await?;
    report.destination_address = format!("{its_proxy_addr}");

    // Reuse the existing EVM-destination ITS verifier on the batch of confirmed txs.
    let verification = super::verify::verify_onchain_evm_its(
        &args.config,
        &args.source_axelar_id,
        &args.destination_axelar_id,
        &format!("{its_proxy_addr}"),
        evm_gateway_addr,
        &args.destination_rpc,
        &mut report.transactions,
    )
    .await?;
    report.verification = Some(verification);

    // Suppress unused warnings while the remaining hooks take shape.
    let _ = account_id_to_hex;

    finish_report(&args, &mut report, test_start)
}

/// Read `(rpc, multisig_address, network_type)` for an XRPL chain from config.
fn read_xrpl_chain_config(
    config: &std::path::Path,
    chain_id: &str,
) -> Result<(String, String, String)> {
    let content =
        std::fs::read_to_string(config).map_err(|e| eyre!("failed to read config: {e}"))?;
    let root: serde_json::Value = serde_json::from_str(&content)?;
    let chain = root
        .pointer(&format!("/chains/{chain_id}"))
        .ok_or_else(|| eyre!("chain '{chain_id}' not found in config"))?;
    // Prefer `rpc` (HTTP JSON-RPC); fall back to `wssRpc` if only WS is present.
    let rpc = chain
        .get("rpc")
        .and_then(|v| v.as_str())
        .or_else(|| chain.get("wssRpc").and_then(|v| v.as_str()))
        .ok_or_else(|| eyre!("no rpc for XRPL chain '{chain_id}'"))?
        .to_string();
    let multisig = chain
        .pointer("/contracts/InterchainTokenService/address")
        .or_else(|| chain.pointer("/contracts/AxelarGateway/address"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("no InterchainTokenService/AxelarGateway address for '{chain_id}'"))?
        .to_string();
    let network_type = chain
        .get("networkType")
        .and_then(|v| v.as_str())
        .unwrap_or("testnet")
        .to_string();
    Ok((rpc, multisig, network_type))
}

/// Load the XRPL main wallet from `--private-key` / `XRPL_PRIVATE_KEY` (hex).
fn load_xrpl_main_wallet(private_key: Option<&str>) -> Result<XrplWallet> {
    let key = private_key
        .map(String::from)
        .or_else(|| std::env::var("XRPL_PRIVATE_KEY").ok())
        .ok_or_else(|| {
            eyre!(
                "XRPL main wallet required. Set XRPL_PRIVATE_KEY env var (32-byte hex secp256k1 secret)."
            )
        })?;
    XrplWallet::from_hex(&key)
}
