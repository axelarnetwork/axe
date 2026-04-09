use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long to wait for an EVM tx receipt before giving up.
const EVM_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);

use alloy::{
    primitives::{Address, Bytes, FixedBytes, U256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
};
use eyre::eyre;
use futures::future::join_all;
use rand::Rng;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::{Mutex, Semaphore};

use super::keypairs;
use super::metrics::{LoadTestReport, TxMetrics};
use super::{
    LoadTestArgs, check_evm_balance, finish_report, read_its_cache, save_its_cache,
    validate_evm_rpc, validate_solana_rpc,
};
use crate::commands::test_its::{
    extract_contract_call_event, extract_token_deployed_event, generate_salt,
};
use crate::cosmos::read_axelar_contract_field;
use crate::evm::{ERC20, InterchainTokenFactory, InterchainTokenService};
use crate::ui;
use crate::utils::read_contract_address;

const TOKEN_NAME: &str = "AXE";
const TOKEN_SYMBOL: &str = "AXE";
const TOKEN_DECIMALS: u8 = 18;

#[cfg(feature = "devnet-amplifier")]
fn default_gas_value_wei(_source_chain: &str) -> u128 {
    0
}
#[cfg(not(feature = "devnet-amplifier"))]
fn default_gas_value_wei(source_chain: &str) -> u128 {
    if source_chain.starts_with("flow") {
        300_000_000_000_000_000
    } else {
        10_000_000_000_000_000
    }
}

const MAX_CONCURRENT_SENDS: usize = 100;
const MAX_RETRIES: u32 = 5;

/// Build Borsh-encoded ITS metadata that triggers the memo program on Solana.
///
/// Format: `[4 bytes metadata_version=0] [1 byte encoding=0x00 Borsh] [borsh payload]`
///
/// The Borsh payload contains the memo string as the execution data and the
/// memo program's counter PDA as a required writable account.
///
/// When `extra_accounts > 0`, additional accounts are appended after the counter
/// PDA to inflate the transaction size and exercise ALT (Address Lookup Table)
/// paths on the relayer. The first extra account is a valid ATA for the ITS
/// token mint (writable); the rest are random pubkeys (read-only).
fn build_its_memo_metadata(
    counter_pda: &Pubkey,
    extra_accounts: u32,
    token_mint_ata: Option<&Pubkey>,
) -> Vec<u8> {
    let mut buf = [0u8; 16];
    rand::thread_rng().fill(&mut buf);
    let memo = format!("axe load test {}", hex::encode(buf));
    let memo_bytes = memo.as_bytes();

    let total_accounts = 1 + extra_accounts;

    // Metadata version (4 bytes, all zero)
    let mut metadata = vec![0u8; 4];
    // Encoding scheme: 0x00 = Borsh
    metadata.push(0x00);
    // Borsh payload: [u32 LE payload_length] [payload] [u32 LE account_count] [accounts...]
    metadata.extend(&(memo_bytes.len() as u32).to_le_bytes());
    metadata.extend_from_slice(memo_bytes);
    // Account count
    metadata.extend(&total_accounts.to_le_bytes());
    // Account 0: counter PDA (writable, not signer)
    metadata.extend_from_slice(&counter_pda.to_bytes());
    metadata.push(0x02); // writable=true, signer=false

    // Extra accounts for ALT testing
    for i in 0..extra_accounts {
        if i == 0 {
            if let Some(ata) = token_mint_ata {
                // First extra account: valid ATA (writable)
                metadata.extend_from_slice(&ata.to_bytes());
                metadata.push(0x02); // writable=true, signer=false
                continue;
            }
        }
        // Remaining: random pubkeys (read-only)
        let mut random_key = [0u8; 32];
        rand::thread_rng().fill(&mut random_key);
        metadata.extend_from_slice(&random_key);
        metadata.push(0x00); // read-only, not signer
    }

    metadata
}

pub async fn run(args: LoadTestArgs, _run_start: Instant) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let evm_rpc_url = args.source_rpc.clone();

    validate_evm_rpc(&evm_rpc_url).await?;
    validate_solana_rpc(&args.destination_rpc).await?;

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
    ui::kv(
        "protocol",
        "ITS with data (interchainTransfer + memo execute)",
    );

    // --- EVM signer ---
    let private_key = args.private_key.as_ref().ok_or_else(|| {
        eyre!("EVM private key required. Set EVM_PRIVATE_KEY env var or use --private-key")
    })?;
    let signer: PrivateKeySigner = private_key.parse()?;
    let deployer_address = signer.address();
    let read_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    check_evm_balance(&read_provider, deployer_address).await?;

    let main_key: [u8; 32] = signer.to_bytes().into();

    #[allow(clippy::float_arithmetic)]
    {
        let balance: u128 = read_provider.get_balance(deployer_address).await?.to();
        let eth = balance as f64 / 1e18;
        ui::kv("wallet", &format!("{deployer_address} ({eth:.6} ETH)"));
    }

    // --- ITS contract addresses ---
    let its_factory_addr = read_contract_address(&args.config, src, "InterchainTokenFactory")?;
    let its_proxy_addr = read_contract_address(&args.config, src, "InterchainTokenService")?;

    ui::address("ITS factory", &format!("{its_factory_addr}"));
    ui::address("ITS service", &format!("{its_proxy_addr}"));

    let write_provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(evm_rpc_url.parse()?);

    // --- Memo program counter PDA ---
    let memo_program_id = super::evm_sender::memo_program_id();
    let (counter_pda, _) = Pubkey::find_program_address(&[b"counter"], &memo_program_id);
    ui::kv("memo program", &memo_program_id.to_string());
    ui::kv("counter PDA", &counter_pda.to_string());

    // --- Gas value ---
    let gas_value_wei: u128 = match &args.gas_value {
        Some(v) => v.parse().map_err(|e| eyre!("invalid --gas-value: {e}"))?,
        None => default_gas_value_wei(src),
    };
    let gas_value = U256::from(gas_value_wei);

    #[allow(clippy::float_arithmetic)]
    {
        ui::kv(
            "gas value",
            &format!(
                "{gas_value_wei} wei ({:.6} ETH)",
                gas_value_wei as f64 / 1e18
            ),
        );
    }

    // --- Token setup (reuses same logic as its_evm_to_sol) ---
    let burst_mode = !(args.tps.is_some() && args.duration_secs.is_some());
    let (num_keys, total_expected) = if burst_mode {
        let n = args.num_txs.max(1) as usize;
        (n, args.num_txs.max(1))
    } else {
        let tps = args.tps.unwrap() as usize;
        let dur = args.duration_secs.unwrap();
        (tps * args.key_cycle as usize, tps as u64 * dur)
    };
    let num_txs = num_keys;
    let amount_per_tx = U256::from(1_000_000_000_000_000_000u128); // 1 token
    let amount_per_key = amount_per_tx * U256::from(100);
    let total_supply = U256::from(1_000_000) * U256::from(1_000_000_000_000_000_000u128);

    let its_service = InterchainTokenService::new(its_proxy_addr, &write_provider);

    let (token_id, token_addr, deploy_message_id) = if let Some(ref tid) = args.token_id {
        let token_id: FixedBytes<32> = tid.parse().map_err(|e| eyre!("invalid --token-id: {e}"))?;
        let addr = its_service
            .interchainTokenAddress(token_id)
            .call()
            .await
            .map_err(|e| eyre!("failed to look up token address for {token_id}: {e}"))?;
        ui::kv("token ID (provided)", &format!("{token_id}"));
        ui::address("token address", &format!("{addr}"));
        (token_id, addr, None)
    } else {
        let cache = read_its_cache(src, dest);
        let cached = cache
            .get("tokenId")
            .and_then(|v| v.as_str())
            .and_then(|tid| tid.parse::<FixedBytes<32>>().ok())
            .and_then(|tid| {
                cache
                    .get("tokenAddress")
                    .and_then(|v| v.as_str())
                    .and_then(|a| a.parse::<Address>().ok())
                    .map(|addr| (tid, addr))
            });

        if let Some((tid, addr)) = cached {
            let token = ERC20::new(addr, &write_provider);
            let needed = amount_per_key * U256::from(num_keys);
            let balance = token
                .balanceOf(deployer_address)
                .call()
                .await
                .unwrap_or_default();
            if balance >= needed {
                ui::info(&format!("reusing cached ITS token: {addr}"));
                ui::kv("token ID (cached)", &format!("{tid}"));
                (tid, addr, None)
            } else {
                ui::warn("cached token supply insufficient, deploying fresh...");
                deploy_its_token(
                    &write_provider,
                    its_factory_addr,
                    deployer_address,
                    dest,
                    total_supply,
                    src,
                    gas_value,
                )
                .await?
            }
        } else {
            deploy_its_token(
                &write_provider,
                its_factory_addr,
                deployer_address,
                dest,
                total_supply,
                src,
                gas_value,
            )
            .await?
        }
    };

    // Wait for the remote deploy to propagate through the hub to Solana.
    if let Some(ref deploy_msg_id) = deploy_message_id {
        super::verify::wait_for_its_remote_deploy_to_solana(
            &args.config,
            src,
            dest,
            deploy_msg_id,
            &args.destination_rpc,
        )
        .await?;
    }

    // --- Extra accounts for ALT testing ---
    let extra_accounts = args.extra_accounts;
    let token_mint_ata = if extra_accounts > 0 {
        // Derive the ITS token mint on Solana and compute an ATA for the memo program.
        let (its_root, _) = crate::solana::find_its_root_pda();
        let (sol_mint, _) = crate::solana::find_interchain_token_pda(&its_root, token_id.as_slice());
        let token_program = Pubkey::from_str_const(
            "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
        );
        let ata_program = Pubkey::from_str_const(
            "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
        );
        let ata = Pubkey::find_program_address(
            &[memo_program_id.as_ref(), token_program.as_ref(), sol_mint.as_ref()],
            &ata_program,
        ).0;
        ui::kv("extra accounts", &extra_accounts.to_string());
        ui::address("first extra (ATA)", &ata.to_string());
        Some(ata)
    } else {
        None
    };

    // --- Derive N EVM signers ---
    let derived = keypairs::derive_evm_signers(&main_key, num_keys)?;
    ui::info(&format!("derived {} EVM signing keys", derived.len()));

    // Fund derived wallets.
    let funding_provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(evm_rpc_url.parse()?);
    let gas_extra_per_key = if burst_mode {
        gas_value_wei
    } else {
        let dur = args.duration_secs.unwrap();
        let rounds = dur.div_ceil(args.key_cycle);
        let buffered = rounds + rounds / 5 + 1;
        gas_value_wei.saturating_mul(buffered as u128)
    };
    keypairs::ensure_funded_evm_with_extra(&funding_provider, &signer, &derived, gas_extra_per_key)
        .await?;

    // --- Distribute ITS tokens to derived wallets ---
    let token_provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(evm_rpc_url.parse()?);
    super::its_evm_to_sol::distribute_tokens(&token_provider, token_addr, &derived, amount_per_key)
        .await?;

    // Destination is the memo program (not a wallet), since we want it to
    // execute with the interchain token.
    let receiver_bytes = Bytes::from(memo_program_id.to_bytes().to_vec());

    // Build the metadata once (counter PDA is the same for all txs, memo varies per tx).
    // We build per-tx in the send function so each tx gets a unique memo.

    // === SUSTAINED MODE ===
    if !burst_mode {
        let tps = args.tps.unwrap() as usize;
        let duration_secs = args.duration_secs.unwrap();
        let key_cycle = args.key_cycle as usize;
        let rpc_url_str = evm_rpc_url.clone();

        let nonce_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
        let mut nonces: Vec<u64> = Vec::with_capacity(num_keys);
        for signer in &derived {
            let n = nonce_provider
                .get_transaction_count(signer.address())
                .await?;
            nonces.push(n);
        }

        let spinner = ui::wait_spinner(&format!(
            "[0/{duration_secs}s] starting sustained ITS-with-data send..."
        ));
        let test_start = Instant::now();
        let dest_chain_s = dest.to_string();
        let counter_pda_clone = counter_pda;
        let ea = extra_accounts;
        let tma = token_mint_ata;

        let make_task: super::sustained::MakeTask =
            Box::new(move |key_idx: usize, nonce: Option<u64>| {
                let dc = dest_chain_s.clone();
                let gv = gas_value;
                let rb = receiver_bytes.clone();
                let amt = amount_per_tx;
                let its_proxy = its_proxy_addr;
                let tid = token_id;
                let url = rpc_url_str.clone();
                let cpda = counter_pda_clone;

                let provider = ProviderBuilder::new()
                    .wallet(derived[key_idx].clone())
                    .connect_http(url.parse().expect("invalid RPC URL"));

                Box::pin(async move {
                    execute_interchain_transfer_with_data(
                        &provider, its_proxy, tid, &dc, &rb, amt, gv, &cpda, ea,
                        tma.as_ref(), nonce,
                    )
                    .await
                })
            });

        let result = super::sustained::run_sustained_loop(
            tps,
            duration_secs,
            key_cycle,
            Some(nonces),
            make_task,
            None,
            spinner,
        )
        .await;

        let mut report = super::sustained::build_sustained_report(
            result,
            src,
            dest,
            &format!("{its_proxy_addr}"),
            total_expected,
            num_keys,
        );

        let verification = super::verify::verify_onchain_solana_its(
            &args.config,
            &args.source_axelar_id,
            &args.destination_axelar_id,
            &format!("{its_proxy_addr}"),
            &args.destination_rpc,
            &mut report.transactions,
        )
        .await?;
        report.verification = Some(verification);

        return finish_report(&args, &mut report, test_start);
    }
    // === END SUSTAINED MODE ===

    // --- Burst mode: parallel sends ---
    let metrics_list: Arc<Mutex<Vec<TxMetrics>>> = Arc::new(Mutex::new(Vec::new()));
    let confirmed_counter = Arc::new(AtomicU64::new(0));
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_SENDS));
    let spinner = ui::wait_spinner(&format!("sending ITS-with-data (0/{num_txs} confirmed)..."));
    let test_start = Instant::now();

    let mut tasks = Vec::with_capacity(num_txs);
    let dest_chain = dest.to_string();

    for derived_signer in &derived {
        let metrics_clone = Arc::clone(&metrics_list);
        let counter = Arc::clone(&confirmed_counter);
        let sem = Arc::clone(&semaphore);
        let sp = spinner.clone();
        let total = num_txs;
        let dc = dest_chain.clone();
        let gv = gas_value;
        let rb = receiver_bytes.clone();
        let amt = amount_per_tx;
        let its_proxy = its_proxy_addr;
        let tid = token_id;
        let cpda = counter_pda;
        let ea = extra_accounts;
        let tma = token_mint_ata;

        let provider = ProviderBuilder::new()
            .wallet(derived_signer.clone())
            .connect_http(evm_rpc_url.parse()?);

        let handle = tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();

            let mut m = None;
            for attempt in 0..=MAX_RETRIES {
                let result = execute_interchain_transfer_with_data(
                    &provider, its_proxy, tid, &dc, &rb, amt, gv, &cpda, ea,
                    tma.as_ref(), None,
                )
                .await;

                if result.success || attempt == MAX_RETRIES {
                    m = Some(result);
                    break;
                }

                let is_rate_limited = result.error.as_deref().is_some_and(|e| e.contains("429"));
                if !is_rate_limited {
                    m = Some(result);
                    break;
                }

                let backoff = Duration::from_secs(1 << attempt);
                tokio::time::sleep(backoff).await;
            }

            let m = m.unwrap();
            if m.success {
                let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
                sp.set_message(format!(
                    "sending ITS-with-data ({done}/{total} confirmed)..."
                ));
            }
            metrics_clone.lock().await.push(m);
        });
        tasks.push(handle);
    }

    let total_submitted = tasks.len() as u64;
    join_all(tasks).await;
    let test_duration = test_start.elapsed().as_secs_f64();

    let confirmed_count = confirmed_counter.load(Ordering::Relaxed);
    spinner.finish_and_clear();
    ui::success(&format!(
        "sent {confirmed_count}/{total_submitted} ITS-with-data confirmed"
    ));

    let metrics = metrics_list.lock().await.clone();
    let total_confirmed = metrics.iter().filter(|m| m.success).count() as u64;
    let total_failed = metrics.iter().filter(|m| !m.success).count() as u64;

    if total_failed > 0 {
        let mut error_counts: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
        for m in metrics.iter().filter(|m| !m.success) {
            let reason = m
                .error
                .as_deref()
                .unwrap_or("unknown")
                .chars()
                .take(120)
                .collect::<String>();
            *error_counts.entry(reason).or_default() += 1;
        }
        for (reason, count) in &error_counts {
            ui::warn(&format!("{count} txs failed: {reason}"));
        }
    }

    let latencies: Vec<u64> = metrics.iter().filter_map(|m| m.latency_ms).collect();

    #[allow(clippy::cast_precision_loss, clippy::float_arithmetic)]
    let mut report = LoadTestReport {
        source_chain: src.to_string(),
        destination_chain: dest.to_string(),
        destination_address: format!("{its_proxy_addr}"),
        protocol: String::new(),
        tps: None,
        duration_secs: None,
        num_txs: args.num_txs,
        num_keys: num_txs,
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

    // --- Verify ---
    let verification = super::verify::verify_onchain_solana_its(
        &args.config,
        &args.source_axelar_id,
        &args.destination_axelar_id,
        &format!("{its_proxy_addr}"),
        &args.destination_rpc,
        &mut report.transactions,
    )
    .await?;
    report.verification = Some(verification);

    finish_report(&args, &mut report, test_start)
}

/// Deploy a new interchain token and its remote counterpart.
#[allow(clippy::too_many_arguments)]
async fn deploy_its_token<P: Provider>(
    provider: &P,
    factory_addr: Address,
    deployer: Address,
    dest_chain: &str,
    total_supply: U256,
    source_chain: &str,
    gas_value: U256,
) -> eyre::Result<(FixedBytes<32>, Address, Option<String>)> {
    let salt = generate_salt();

    ui::info("deploying new ITS token...");
    ui::kv("name", TOKEN_NAME);
    ui::kv("symbol", TOKEN_SYMBOL);
    ui::kv("decimals", &TOKEN_DECIMALS.to_string());
    ui::kv("supply", &format!("{total_supply}"));

    let factory = InterchainTokenFactory::new(factory_addr, provider);

    let deploy_call = factory
        .deployInterchainToken(
            salt,
            TOKEN_NAME.to_string(),
            TOKEN_SYMBOL.to_string(),
            TOKEN_DECIMALS,
            total_supply,
            deployer,
        )
        .value(U256::ZERO);

    let pending = deploy_call.send().await?;
    let tx_hash = *pending.tx_hash();
    ui::tx_hash("deploy tx", &format!("{tx_hash}"));

    let receipt = tokio::time::timeout(Duration::from_secs(120), pending.get_receipt())
        .await
        .map_err(|_| eyre!("deploy tx timed out after 120s"))??;

    let (token_id, token_addr) = extract_token_deployed_event(&receipt)?;
    ui::kv("token ID", &format!("{token_id}"));
    ui::address("token address", &format!("{token_addr}"));

    ui::info(&format!("deploying remote token to {dest_chain}..."));

    let remote_call = factory
        .deployRemoteInterchainToken(salt, dest_chain.to_string(), gas_value)
        .value(gas_value);

    let pending = remote_call.send().await?;
    let tx_hash = *pending.tx_hash();
    ui::tx_hash("remote deploy tx", &format!("{tx_hash}"));

    let receipt = tokio::time::timeout(Duration::from_secs(120), pending.get_receipt())
        .await
        .map_err(|_| eyre!("remote deploy tx timed out after 120s"))??;

    ui::success(&format!(
        "remote deploy confirmed in block {}",
        receipt.block_number.unwrap_or(0)
    ));

    let deploy_message_id = match extract_contract_call_event(&receipt) {
        Ok((event_index, _, _, _, _)) => {
            let msg_id = format!("{tx_hash:#x}-{event_index}");
            ui::kv("remote deploy message ID", &msg_id);
            Some(msg_id)
        }
        Err(_) => None,
    };

    let cache = serde_json::json!({
        "tokenId": format!("{token_id}"),
        "tokenAddress": format!("{token_addr}"),
        "salt": format!("{salt}"),
    });
    save_its_cache(source_chain, dest_chain, &cache)?;

    Ok((token_id, token_addr, deploy_message_id))
}

/// Send a single interchainTransfer with metadata that triggers the memo program.
#[allow(clippy::too_many_arguments)]
async fn execute_interchain_transfer_with_data<P: Provider>(
    provider: &P,
    its_proxy: Address,
    token_id: FixedBytes<32>,
    dest_chain: &str,
    receiver_bytes: &Bytes,
    amount: U256,
    gas_value: U256,
    counter_pda: &Pubkey,
    extra_accounts: u32,
    token_mint_ata: Option<&Pubkey>,
    explicit_nonce: Option<u64>,
) -> TxMetrics {
    let submit_start = Instant::now();

    // Build unique metadata per tx (random memo string)
    let metadata = Bytes::from(build_its_memo_metadata(counter_pda, extra_accounts, token_mint_ata));

    let its = InterchainTokenService::new(its_proxy, provider);
    let base_call = its
        .interchainTransfer(
            token_id,
            dest_chain.to_string(),
            receiver_bytes.clone(),
            amount,
            metadata,
            gas_value,
        )
        .value(gas_value);
    let call = match explicit_nonce {
        Some(n) => base_call.nonce(n),
        None => base_call,
    };

    match call.send().await {
        Ok(pending) => {
            let tx_hash = *pending.tx_hash();
            match tokio::time::timeout(EVM_RECEIPT_TIMEOUT, pending.get_receipt()).await {
                Ok(Ok(receipt)) => {
                    #[allow(clippy::cast_possible_truncation)]
                    let latency_ms = submit_start.elapsed().as_millis() as u64;

                    match extract_contract_call_event(&receipt) {
                        Ok((
                            event_index,
                            _payload,
                            payload_hash_bytes,
                            dest_chain,
                            dest_address,
                        )) => {
                            let message_id = format!("{tx_hash:#x}-{event_index}");
                            let source_address = format!("{its_proxy}");
                            let payload_hash = alloy::hex::encode(payload_hash_bytes.as_slice());

                            TxMetrics {
                                signature: message_id,
                                submit_time_ms: 0,
                                confirm_time_ms: Some(latency_ms),
                                latency_ms: Some(latency_ms),
                                compute_units: Some(receipt.gas_used),
                                slot: receipt.block_number,
                                success: true,
                                error: None,
                                payload: Vec::new(),
                                payload_hash,
                                source_address,
                                gmp_destination_chain: dest_chain,
                                gmp_destination_address: dest_address,
                                send_instant: Some(submit_start),
                                amplifier_timing: None,
                            }
                        }
                        Err(e) => {
                            make_failure(submit_start, &format!("no ContractCall event: {e}"))
                        }
                    }
                }
                Ok(Err(e)) => make_failure_with_hash(submit_start, &e.to_string(), Some(tx_hash)),
                Err(_) => make_failure_with_hash(submit_start, "tx timed out", Some(tx_hash)),
            }
        }
        Err(e) => make_failure(submit_start, &e.to_string()),
    }
}

fn make_failure(submit_start: Instant, error: &str) -> TxMetrics {
    make_failure_with_hash(submit_start, error, None)
}

fn make_failure_with_hash(
    submit_start: Instant,
    error: &str,
    tx_hash: Option<alloy::primitives::TxHash>,
) -> TxMetrics {
    #[allow(clippy::cast_possible_truncation)]
    let elapsed_ms = submit_start.elapsed().as_millis() as u64;
    TxMetrics {
        signature: tx_hash.map_or_else(String::new, |h| format!("{h:#x}")),
        submit_time_ms: elapsed_ms,
        confirm_time_ms: None,
        latency_ms: None,
        compute_units: None,
        slot: None,
        success: false,
        error: Some(error.to_string()),
        payload: Vec::new(),
        payload_hash: String::new(),
        source_address: String::new(),
        gmp_destination_chain: String::new(),
        gmp_destination_address: String::new(),
        send_instant: None,
        amplifier_timing: None,
    }
}
