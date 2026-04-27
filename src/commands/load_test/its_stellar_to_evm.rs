//! Stellar -> any EVM ITS load test.
//!
//! Mirrors `its_sol_to_evm.rs`:
//!   1. Deploy the AXE interchain token on Stellar (or reuse cached token_id)
//!   2. Register it on the EVM destination via `deploy_remote_interchain_token`
//!   3. Wait for the remote-deploy message to land on the EVM ITS proxy
//!   4. Distribute AXE balances to ephemeral Stellar wallets
//!   5. Fire `interchain_transfer` calls (burst or sustained)
//!   6. Verify through Amplifier (voted → hub_approved → routed → approved → executed)

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use eyre::{Result, eyre};
use futures::future::join_all;
use rand::RngCore;
use tokio::sync::Mutex;

use super::metrics::{LoadTestReport, TxMetrics};
use super::sustained;
use super::{LoadTestArgs, finish_report, read_its_cache, save_its_cache, validate_evm_rpc};
use crate::cosmos::read_axelar_contract_field;
use crate::stellar::{StellarClient, StellarWallet};
use crate::ui;
use crate::utils::read_contract_address;

/// AXE token parameters on Stellar — match the EVM/Solana siblings so the
/// human-facing name is consistent across runs.
const TOKEN_NAME: &str = "AXE";
const TOKEN_SYMBOL: &str = "AXE";
/// 7 decimals matches Stellar's native XLM convention. Token amounts on the
/// destination chain are scaled by ITS during routing.
const TOKEN_DECIMALS: u32 = 7;

/// Per-tx transfer amount (token units). With 7 decimals, this is 1 AXE.
const AMOUNT_PER_TX: u64 = 10_000_000;
/// Distribute 100x per key so cached tokens last across many runs.
const AMOUNT_PER_KEY: u64 = AMOUNT_PER_TX * 100;
/// Initial supply minted to the deployer at deploy time. Plenty for many
/// runs without redeploying.
const INITIAL_SUPPLY: u128 = 1_000_000 * 10_000_000;

/// Default cross-chain gas payment in stroops (10 XLM). Matches the GMP
/// runner's default — overridable via `--gas-value`.
const DEFAULT_GAS_STROOPS: u64 = 100_000_000;

pub async fn run(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let evm_rpc_url = args.destination_rpc.clone();
    validate_evm_rpc(&evm_rpc_url).await?;

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
    ui::kv("protocol", "ITS (interchainTransfer via hub)");

    // --- Stellar config + main wallet ---
    let stellar_rpc = &args.source_rpc;
    let network_type = super::read_stellar_network_type(&args.config, src)?;
    let stellar_client = StellarClient::new(stellar_rpc, &network_type)?;
    let stellar_its_addr =
        super::read_stellar_contract_address(&args.config, src, "InterchainTokenService")?;
    let stellar_gateway_addr =
        super::read_stellar_contract_address(&args.config, src, "AxelarGateway")?;
    let stellar_xlm_addr = super::read_stellar_token_address(&args.config, src)?;
    ui::address("Stellar ITS", &stellar_its_addr);
    ui::address("Stellar AxelarGateway", &stellar_gateway_addr);
    ui::address("Stellar XLM token", &stellar_xlm_addr);

    let main_wallet = super::load_stellar_main_wallet(args.private_key.as_deref())?;
    ui::kv("Stellar wallet", &main_wallet.address());

    // For ITS the main wallet itself signs deploy + distribution txs, so it
    // must be activated. (GMP doesn't need this — ephemeral wallets sign
    // there.) Friendbot it on testnet/futurenet; otherwise leave to the user.
    let use_friendbot = matches!(network_type.as_str(), "testnet" | "futurenet");
    if stellar_client
        .account_sequence(&main_wallet.address())
        .await?
        .is_none()
    {
        if use_friendbot {
            ui::info("activating Stellar main wallet via Friendbot...");
            stellar_client
                .friendbot_fund(&main_wallet.address())
                .await?;
            ui::success("main wallet activated");
        } else {
            eyre::bail!(
                "Stellar main wallet {} is not activated — fund it manually (need ≥ 2 XLM \
                 base reserve plus enough for token deploys + per-key distribution).",
                main_wallet.address()
            );
        }
    }

    // --- EVM destination ITS + gateway ---
    let evm_its_addr = read_contract_address(&args.config, dest, "InterchainTokenService")?;
    ui::address("destination ITS", &format!("{evm_its_addr}"));
    let dest_address_bytes = evm_its_addr.as_slice().to_vec();

    let evm_gateway_addr = read_contract_address(&args.config, dest, "AxelarGateway")?;
    ui::address("EVM gateway", &format!("{evm_gateway_addr}"));

    // --- Gas (XLM stroops) ---
    let gas_stroops: u64 = match &args.gas_value {
        Some(v) => v.parse().map_err(|e| eyre!("invalid --gas-value: {e}"))?,
        None => DEFAULT_GAS_STROOPS,
    };
    ui::kv(
        "gas",
        &format!(
            "{gas_stroops} stroops ({:.4} XLM)",
            gas_stroops as f64 / 10_000_000.0
        ),
    );

    // --- Burst vs sustained ---
    let burst_mode = !(args.tps.is_some() && args.duration_secs.is_some());
    let (num_keys, total_expected) = if burst_mode {
        let n = args.num_txs.max(1) as usize;
        (n, args.num_txs.max(1))
    } else {
        let tps = args.tps.unwrap() as usize;
        let dur = args.duration_secs.unwrap();
        (tps * args.key_cycle as usize, tps as u64 * dur)
    };

    // --- Token setup (cache → reuse / deploy fresh) ---
    let (token_id, _salt, token_address) = setup_its_token(
        &stellar_client,
        &main_wallet,
        &stellar_its_addr,
        &stellar_gateway_addr,
        &stellar_xlm_addr,
        gas_stroops,
        src,
        dest,
        &args.destination_axelar_id,
        args.token_id.as_deref(),
        &args.config,
        evm_gateway_addr,
        &evm_rpc_url,
        num_keys,
    )
    .await?;
    ui::kv("token ID", &hex::encode(token_id));
    ui::address("token contract (Stellar)", &token_address);

    // --- Derive + activate ephemeral wallets ---
    ui::info(&format!("deriving {num_keys} Stellar keys..."));
    let main_seed = main_wallet.signing_key.to_bytes();
    let wallets = super::stellar_sender::derive_wallets(&main_seed, num_keys)?;
    let _ = main_seed;
    super::stellar_sender::ensure_funded(&stellar_client, &wallets, use_friendbot).await?;

    // --- Distribute AXE to ephemeral wallets ---
    let amount_per_key = if burst_mode {
        AMOUNT_PER_KEY
    } else {
        let txs_per_key = args.duration_secs.unwrap().div_ceil(args.key_cycle);
        AMOUNT_PER_TX.saturating_mul(txs_per_key).saturating_mul(2)
    };
    distribute_token_balances(
        &stellar_client,
        &main_wallet,
        &token_address,
        &wallets,
        amount_per_key as u128,
    )
    .await?;

    // --- ITS hub routing info (used in TxMetrics so the verifier can find
    //     the second-leg message via the hub's outgoing_messages) ---
    let axelarnet_gw_addr =
        read_axelar_contract_field(&args.config, "/axelar/contracts/AxelarnetGateway/address")?;

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
        let vdest_rpc = evm_rpc_url.clone();
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
            "[0/{duration_secs}s] starting sustained Stellar ITS send..."
        ));
        let _ = spinner_tx.send(spinner.clone());

        let test_start = Instant::now();
        let result = run_sustained_loop(
            &stellar_client,
            wallets,
            stellar_its_addr.clone(),
            stellar_gateway_addr.clone(),
            token_id,
            args.destination_axelar_id.clone(),
            dest_address_bytes.clone(),
            stellar_xlm_addr.clone(),
            gas_stroops,
            tps_n,
            duration_secs,
            key_cycle,
            Some(verify_tx),
            Some(send_done),
            spinner,
            axelarnet_gw_addr.clone(),
        )
        .await;

        let mut report = sustained::build_sustained_report(
            result,
            src,
            dest,
            &format!("{evm_its_addr}"),
            total_expected,
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
    let metrics_list: Arc<Mutex<Vec<TxMetrics>>> = Arc::new(Mutex::new(Vec::new()));
    let confirmed = Arc::new(AtomicU64::new(0));
    let spinner = ui::wait_spinner(&format!("sending (0/{num_keys} confirmed)..."));

    let client = Arc::new(stellar_client.clone());
    let stellar_its_arc = Arc::new(stellar_its_addr.clone());
    let stellar_gw_arc = Arc::new(stellar_gateway_addr.clone());
    let stellar_xlm_arc = Arc::new(stellar_xlm_addr.clone());
    let dest_chain_arc = Arc::new(args.destination_axelar_id.clone());
    let dest_addr_arc = Arc::new(dest_address_bytes.clone());
    let axelarnet_gw_arc = Arc::new(axelarnet_gw_addr.clone());

    let mut tasks = Vec::with_capacity(num_keys);
    for w in wallets {
        let c = Arc::clone(&client);
        let its = Arc::clone(&stellar_its_arc);
        let gw = Arc::clone(&stellar_gw_arc);
        let xlm = Arc::clone(&stellar_xlm_arc);
        let dc = Arc::clone(&dest_chain_arc);
        let da = Arc::clone(&dest_addr_arc);
        let gmp_dest_addr = Arc::clone(&axelarnet_gw_arc);
        let metrics_clone = Arc::clone(&metrics_list);
        let counter = Arc::clone(&confirmed);
        let sp = spinner.clone();
        let total = num_keys;

        let handle = tokio::spawn(async move {
            let m = submit_its_transfer(
                &c,
                &w,
                &its,
                &gw,
                token_id,
                &dc,
                &da,
                &xlm,
                gas_stroops,
                AMOUNT_PER_TX as u128,
                &gmp_dest_addr,
            )
            .await;
            if m.success {
                let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
                sp.set_message(format!("sending ({done}/{total} confirmed)..."));
            }
            metrics_clone.lock().await.push(m);
        });
        tasks.push(handle);
    }

    let total_submitted = tasks.len() as u64;
    join_all(tasks).await;
    let test_duration = test_start.elapsed().as_secs_f64();
    let confirmed_count = confirmed.load(Ordering::Relaxed);
    spinner.finish_and_clear();
    ui::success(&format!(
        "sent {confirmed_count}/{total_submitted} confirmed"
    ));

    let metrics = metrics_list.lock().await.clone();
    let total_confirmed = metrics.iter().filter(|m| m.success).count() as u64;
    let total_failed = metrics.iter().filter(|m| !m.success).count() as u64;
    let latencies: Vec<u64> = metrics.iter().filter_map(|m| m.latency_ms).collect();

    #[allow(clippy::cast_precision_loss, clippy::float_arithmetic)]
    let mut report = LoadTestReport {
        source_chain: src.to_string(),
        destination_chain: dest.to_string(),
        destination_address: format!("{evm_its_addr}"),
        protocol: String::new(),
        tps: None,
        duration_secs: None,
        num_txs: total_submitted,
        num_keys,
        total_submitted,
        total_confirmed,
        total_failed,
        test_duration_secs: test_duration,
        tps_submitted: if test_duration > 0.0 {
            total_submitted as f64 / test_duration
        } else {
            0.0
        },
        tps_confirmed: if test_duration > 0.0 {
            total_confirmed as f64 / test_duration
        } else {
            0.0
        },
        landing_rate: if total_submitted > 0 {
            total_confirmed as f64 / total_submitted as f64
        } else {
            0.0
        },
        avg_latency_ms: if latencies.is_empty() {
            None
        } else {
            Some(latencies.iter().sum::<u64>() as f64 / latencies.len() as f64)
        },
        min_latency_ms: latencies.iter().min().copied(),
        max_latency_ms: latencies.iter().max().copied(),
        avg_compute_units: None,
        min_compute_units: None,
        max_compute_units: None,
        verification: None,
        transactions: metrics,
    };

    let verification = super::verify::verify_onchain_evm_its(
        &args.config,
        &args.source_axelar_id,
        &args.destination_axelar_id,
        &format!("{evm_its_addr}"),
        evm_gateway_addr,
        &evm_rpc_url,
        &mut report.transactions,
    )
    .await?;
    report.verification = Some(verification);

    finish_report(&args, &mut report, test_start)
}

// ---------------------------------------------------------------------------
// Token setup
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn setup_its_token(
    client: &StellarClient,
    main_wallet: &StellarWallet,
    its_contract: &str,
    gateway_contract: &str,
    xlm_token: &str,
    gas_stroops: u64,
    src: &str,
    dest: &str,
    dest_axelar_id: &str,
    token_id_override: Option<&str>,
    config: &std::path::Path,
    evm_gateway_addr: alloy::primitives::Address,
    evm_rpc_url: &str,
    num_txs: usize,
) -> Result<([u8; 32], [u8; 32], String)> {
    if let Some(tid_hex) = token_id_override {
        let tid_bytes = hex::decode(tid_hex.strip_prefix("0x").unwrap_or(tid_hex))
            .map_err(|e| eyre!("invalid --token-id: {e}"))?;
        if tid_bytes.len() != 32 {
            return Err(eyre!("--token-id must be 32 bytes"));
        }
        let mut token_id = [0u8; 32];
        token_id.copy_from_slice(&tid_bytes);
        let token_addr = client
            .its_query_token_address(main_wallet, its_contract, token_id)
            .await?
            .ok_or_else(|| eyre!("token id {tid_hex} not registered on Stellar ITS"))?;
        ui::kv("token ID (provided)", tid_hex);
        return Ok((token_id, [0u8; 32], token_addr));
    }

    let cache = read_its_cache(src, dest);
    if let Some(tid_hex) = cache.get("tokenId").and_then(|v| v.as_str())
        && let Some(salt_hex) = cache.get("salt").and_then(|v| v.as_str())
    {
        let tid_bytes = hex::decode(tid_hex.strip_prefix("0x").unwrap_or(tid_hex)).ok();
        let salt_bytes_v = hex::decode(salt_hex.strip_prefix("0x").unwrap_or(salt_hex)).ok();
        if let (Some(tid), Some(s)) = (tid_bytes, salt_bytes_v)
            && tid.len() == 32
            && s.len() == 32
        {
            let mut token_id = [0u8; 32];
            token_id.copy_from_slice(&tid);
            let mut salt = [0u8; 32];
            salt.copy_from_slice(&s);
            // Verify token still exists + deployer has enough supply.
            if let Ok(Some(token_addr)) = client
                .its_query_token_address(main_wallet, its_contract, token_id)
                .await
            {
                let needed = AMOUNT_PER_KEY.saturating_mul(num_txs as u64) as u128;
                let bal = client
                    .token_balance(main_wallet, &token_addr, &main_wallet.public_key_bytes)
                    .await
                    .unwrap_or(0);
                if bal >= needed {
                    ui::info(&format!("reusing cached ITS token: {token_addr}"));
                    return Ok((token_id, salt, token_addr));
                }
                ui::warn(&format!(
                    "cached AXE token has insufficient supply ({bal} < {needed}), deploying fresh..."
                ));
            } else {
                ui::warn(
                    "cached AXE token no longer registered on Stellar ITS, deploying fresh...",
                );
            }
        }
    }

    // Deploy fresh.
    let mut salt = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut salt);

    ui::info("deploying new ITS token on Stellar...");
    ui::kv("name", TOKEN_NAME);
    ui::kv("symbol", TOKEN_SYMBOL);
    ui::kv("decimals", &TOKEN_DECIMALS.to_string());
    ui::kv("supply", &INITIAL_SUPPLY.to_string());

    let (deploy_invoked, token_id_opt) = client
        .its_deploy_interchain_token(
            main_wallet,
            its_contract,
            salt,
            TOKEN_DECIMALS,
            TOKEN_NAME,
            TOKEN_SYMBOL,
            INITIAL_SUPPLY,
        )
        .await?;
    if !deploy_invoked.success {
        return Err(eyre!("Stellar deploy_interchain_token failed"));
    }
    let token_id =
        token_id_opt.ok_or_else(|| eyre!("deploy_interchain_token returned no token_id"))?;
    ui::tx_hash("Stellar deploy", &deploy_invoked.tx_hash_hex);
    ui::kv("token ID", &hex::encode(token_id));

    let token_address = client
        .its_query_token_address(main_wallet, its_contract, token_id)
        .await?
        .ok_or_else(|| eyre!("could not resolve interchain_token_address after deploy"))?;
    ui::address("token contract", &token_address);

    // Register on EVM destination via ITS hub.
    ui::info(&format!("deploying remote AXE token to {dest}..."));
    let remote_invoked = client
        .its_deploy_remote_interchain_token(
            main_wallet,
            its_contract,
            gateway_contract,
            salt,
            dest_axelar_id,
            xlm_token,
            gas_stroops,
        )
        .await?;
    if !remote_invoked.success {
        return Err(eyre!("Stellar deploy_remote_interchain_token failed"));
    }
    ui::tx_hash("Stellar remote-deploy", &remote_invoked.tx_hash_hex);
    let event_index = remote_invoked.event_index.unwrap_or(0);
    let deploy_message_id = format!(
        "0x{}-{event_index}",
        remote_invoked.tx_hash_hex.to_lowercase()
    );

    // Wait for it to land on EVM.
    super::verify::wait_for_its_remote_deploy(
        config,
        &super::axelar_id_for_chain(config, src)?,
        dest_axelar_id,
        &deploy_message_id,
        evm_gateway_addr,
        evm_rpc_url,
    )
    .await?;

    // Cache.
    let mut cache = cache;
    cache["tokenId"] = serde_json::json!(format!("0x{}", hex::encode(token_id)));
    cache["salt"] = serde_json::json!(format!("0x{}", hex::encode(salt)));
    cache["tokenAddress"] = serde_json::json!(token_address);
    save_its_cache(src, dest, &cache)?;

    Ok((token_id, salt, token_address))
}

// ---------------------------------------------------------------------------
// Distribution
// ---------------------------------------------------------------------------

async fn distribute_token_balances(
    client: &StellarClient,
    main_wallet: &StellarWallet,
    token_contract: &str,
    wallets: &[StellarWallet],
    amount_per_key: u128,
) -> Result<()> {
    // First, see who needs topping up (skip wallets that already have enough).
    let pb_check = indicatif::ProgressBar::new(wallets.len() as u64);
    pb_check.set_style(
        indicatif::ProgressStyle::with_template("  {bar:40.cyan/dim} {pos}/{len} balances checked")
            .unwrap()
            .progress_chars("=> "),
    );
    let mut to_fund: Vec<usize> = Vec::new();
    for (i, w) in wallets.iter().enumerate() {
        let bal = client
            .token_balance(main_wallet, token_contract, &w.public_key_bytes)
            .await
            .unwrap_or(0);
        if bal < amount_per_key {
            to_fund.push(i);
        }
        pb_check.inc(1);
    }
    pb_check.finish_and_clear();

    if to_fund.is_empty() {
        ui::success(&format!(
            "all {} ephemeral wallets already hold ≥ {amount_per_key} AXE",
            wallets.len()
        ));
        return Ok(());
    }

    ui::info(&format!(
        "distributing AXE to {}/{} keys...",
        to_fund.len(),
        wallets.len()
    ));
    let pb = indicatif::ProgressBar::new(to_fund.len() as u64);
    pb.set_style(
        indicatif::ProgressStyle::with_template("  {bar:40.cyan/dim} {pos}/{len} keys funded")
            .unwrap()
            .progress_chars("=> "),
    );
    for &i in &to_fund {
        let invoked = client
            .token_transfer(
                main_wallet,
                token_contract,
                &wallets[i].public_key_bytes,
                amount_per_key,
            )
            .await?;
        if !invoked.success {
            return Err(eyre!(
                "AXE transfer failed for key {i} (tx {})",
                invoked.tx_hash_hex
            ));
        }
        pb.inc(1);
    }
    pb.finish_and_clear();
    ui::success(&format!(
        "distributed AXE to {} ephemeral keys",
        to_fund.len()
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// Single ITS transfer + sustained loop wrapper
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn submit_its_transfer(
    client: &StellarClient,
    wallet: &StellarWallet,
    its_contract: &str,
    gateway_contract: &str,
    token_id: [u8; 32],
    destination_chain: &str,
    destination_address_bytes: &[u8],
    gas_token: &str,
    gas_amount_stroops: u64,
    transfer_amount: u128,
    gmp_dest_address: &str,
) -> TxMetrics {
    let submit_start = Instant::now();
    // ITS emits the `ContractCall` event from the AxelarGateway contract,
    // so the message's source_address (as recorded by VotingVerifier) is the
    // ITS contract address (which calls the gateway). Match that.
    let source_addr = its_contract.to_string();
    match client
        .its_interchain_transfer(
            wallet,
            its_contract,
            gateway_contract,
            token_id,
            destination_chain,
            destination_address_bytes,
            transfer_amount,
            None,
            gas_token,
            gas_amount_stroops,
        )
        .await
    {
        Ok(invoked) => {
            #[allow(clippy::cast_possible_truncation)]
            let submit_time_ms = submit_start.elapsed().as_millis() as u64;
            let event_index = invoked.event_index.unwrap_or(0);
            let message_id = format!("0x{}-{event_index}", invoked.tx_hash_hex.to_lowercase());
            TxMetrics {
                signature: message_id,
                submit_time_ms,
                confirm_time_ms: Some(submit_time_ms),
                latency_ms: Some(submit_time_ms),
                compute_units: None,
                slot: None,
                success: invoked.success,
                error: if invoked.success {
                    None
                } else {
                    Some("interchain_transfer reverted".to_string())
                },
                payload: Vec::new(),
                payload_hash: String::new(),
                source_address: source_addr,
                gmp_destination_chain: "axelar".to_string(),
                gmp_destination_address: gmp_dest_address.to_string(),
                send_instant: Some(submit_start),
                amplifier_timing: None,
            }
        }
        Err(e) => {
            #[allow(clippy::cast_possible_truncation)]
            let elapsed_ms = submit_start.elapsed().as_millis() as u64;
            TxMetrics {
                signature: String::new(),
                submit_time_ms: elapsed_ms,
                confirm_time_ms: None,
                latency_ms: None,
                compute_units: None,
                slot: None,
                success: false,
                error: Some(e.to_string()),
                payload: Vec::new(),
                payload_hash: String::new(),
                source_address: source_addr,
                gmp_destination_chain: String::new(),
                gmp_destination_address: String::new(),
                send_instant: None,
                amplifier_timing: None,
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_sustained_loop(
    client: &StellarClient,
    wallets: Vec<StellarWallet>,
    its_contract: String,
    gateway_contract: String,
    token_id: [u8; 32],
    destination_chain: String,
    destination_address_bytes: Vec<u8>,
    gas_token: String,
    gas_stroops: u64,
    tps: usize,
    duration_secs: u64,
    key_cycle: usize,
    verify_tx: Option<tokio::sync::mpsc::UnboundedSender<super::verify::PendingTx>>,
    send_done: Option<Arc<AtomicBool>>,
    spinner: indicatif::ProgressBar,
    axelarnet_gw_addr: String,
) -> sustained::SustainedResult {
    let client = Arc::new(client.clone());
    let wallets = Arc::new(wallets);
    let its = Arc::new(its_contract);
    let gw = Arc::new(gateway_contract);
    let dc = Arc::new(destination_chain);
    let da = Arc::new(destination_address_bytes);
    let xlm = Arc::new(gas_token);
    let gmp_dst = Arc::new(axelarnet_gw_addr);

    let make_task: sustained::MakeTask = Box::new(move |key_idx: usize, _nonce: Option<u64>| {
        let c = Arc::clone(&client);
        let ws = Arc::clone(&wallets);
        let its = Arc::clone(&its);
        let gw = Arc::clone(&gw);
        let dc = Arc::clone(&dc);
        let da = Arc::clone(&da);
        let xlm = Arc::clone(&xlm);
        let gmp_dst = Arc::clone(&gmp_dst);
        let vtx = verify_tx.clone();

        Box::pin(async move {
            let wallet = &ws[key_idx % ws.len()];
            let m = submit_its_transfer(
                &c,
                wallet,
                &its,
                &gw,
                token_id,
                &dc,
                &da,
                &xlm,
                gas_stroops,
                AMOUNT_PER_TX as u128,
                &gmp_dst,
            )
            .await;
            if m.success
                && let Some(ref tx_sender) = vtx
            {
                // ITS pipeline: starts at Voted (Stellar VotingVerifier).
                let pending = super::verify::tx_to_pending_its(&m, true);
                let _ = tx_sender.send(pending);
            }
            m
        })
    });

    sustained::run_sustained_loop(
        tps,
        duration_secs,
        key_cycle,
        None,
        make_task,
        send_done,
        spinner,
    )
    .await
}
