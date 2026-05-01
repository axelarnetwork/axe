use std::path::{Path, PathBuf};
use std::time::Instant;

use alloy::{
    primitives::{Address, Bytes, FixedBytes, U256, keccak256},
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
    sol_types::{SolEvent, SolValue},
};
use eyre::Result;
use serde_json::json;
use solana_sdk::signer::Signer as SolSigner;

use crate::cli::resolve_axelar_id;
use crate::commands::test_helpers::{
    end_poll_with_retry, execute_on_axelarnet_gateway, extract_event_attr,
    route_messages_with_retry, submit_verify_messages_amplifier, wait_for_poll_votes,
    wait_for_proof,
};
use crate::cosmos::{
    SecondLegInfo, build_execute_msg_any, check_cosmos_routed, check_hub_approved,
    derive_axelar_wallet, discover_second_leg, read_axelar_config, read_axelar_contract_field,
    read_axelar_rpc, sign_and_broadcast_cosmos_tx,
};
use crate::evm::{
    AxelarAmplifierGateway, ContractCall, ERC20, InterchainToken, InterchainTokenDeployed,
    InterchainTokenFactory, InterchainTokenService, Ownable,
};
use crate::preflight;
use crate::state::read_state;
use crate::timing::{
    AMPLIFIER_POLL_ATTEMPTS_5MIN, AMPLIFIER_POLL_ATTEMPTS_10MIN, AMPLIFIER_POLL_INTERVAL,
    DEST_CHAIN_POLL_ATTEMPTS, DEST_CHAIN_POLL_INTERVAL,
};
use crate::ui;
use crate::utils::read_contract_address;

const TOTAL_STEPS: usize = 10;

// Destination chain (Amplifier chain with an active relayer)
const DEST_CHAIN: &str = "flow";

// Token parameters live in `crate::types::EVM_LEGACY_SPEC` (legacy EVM-direct
// `run`) and `ITS_CONFIG_SPEC` (config-mode `run_config`).

pub async fn run(axelar_id: Option<String>) -> Result<()> {
    let axelar_id = resolve_axelar_id(axelar_id)?;
    let state = read_state(&axelar_id)?;
    let start = Instant::now();

    let rpc_url = state.rpc_url.clone();
    let target_json = state.target_json.clone();

    let private_key = state
        .deployer_private_key
        .clone()
        .ok_or_else(|| eyre::eyre!("no deployerPrivateKey in state"))?;

    let signer: PrivateKeySigner = private_key.parse()?;
    let deployer_address = signer.address();
    let provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(rpc_url.parse()?);

    // --- Pre-flight: check deployer balance ---
    let token_symbol = std::fs::read_to_string(&target_json)
        .ok()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        .and_then(|root| {
            root.pointer(&format!("/chains/{axelar_id}/tokenSymbol"))
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| "ETH".to_string());
    preflight::check_evm_balances(&rpc_url, &[("deployer", deployer_address)], &token_symbol)
        .await?;

    let its_factory_addr =
        read_contract_address(&target_json, &axelar_id, "InterchainTokenFactory")?;
    let its_proxy_addr = read_contract_address(&target_json, &axelar_id, "InterchainTokenService")?;

    // Read destination chain config from testnet.json
    let dest_rpc = read_axelar_contract_field(&target_json, &format!("/chains/{DEST_CHAIN}/rpc"))?;
    let dest_its_addr = read_contract_address(&target_json, DEST_CHAIN, "InterchainTokenService")?;

    ui::section(&format!("ITS Test: {axelar_id} → {DEST_CHAIN}"));
    ui::address("deployer", &format!("{deployer_address}"));
    ui::address("ITS factory", &format!("{its_factory_addr}"));
    ui::address("ITS proxy", &format!("{its_proxy_addr}"));

    // ── Pre-flight: check chain trust ────────────────────────────────────
    let its_service = InterchainTokenService::new(its_proxy_addr, &provider);
    let trusted = its_service
        .isTrustedChain(DEST_CHAIN.to_string())
        .call()
        .await
        .unwrap_or_default();

    if !trusted {
        print_untrusted_chain_remediation(
            &axelar_id,
            its_proxy_addr,
            dest_its_addr,
            &rpc_url,
            &dest_rpc,
            &provider,
        )
        .await?;
        return Ok(());
    }
    ui::success(&format!("\"{DEST_CHAIN}\" is trusted on {axelar_id} ITS"));

    // ── Step 1: Deploy interchain token locally ─────────────────────────
    ui::step_header(1, TOTAL_STEPS, "Deploy interchain token");

    let salt = generate_salt();
    let spec = crate::types::EVM_LEGACY_SPEC;
    let initial_supply = U256::from(1000u64) * U256::from(10u64).pow(U256::from(spec.decimals));

    ui::kv("name", spec.name);
    ui::kv("symbol", spec.symbol);
    ui::kv("decimals", &spec.decimals.to_string());
    ui::kv("initial supply", &format!("{initial_supply}"));
    ui::kv("salt", &format!("{salt}"));

    let factory = InterchainTokenFactory::new(its_factory_addr, &provider);
    let deploy_call = factory
        .deployInterchainToken(
            salt,
            spec.name.to_string(),
            spec.symbol.to_string(),
            spec.decimals,
            initial_supply,
            deployer_address,
        )
        .value(U256::ZERO);

    let pending = deploy_call.send().await?;
    let receipt = crate::evm::broadcast_and_log(pending, "tx").await?;

    // Extract tokenId from InterchainTokenDeployed event logs
    let (token_id, local_token_addr) = extract_token_deployed_event(&receipt)?;
    ui::kv("tokenId", &format!("{token_id}"));
    ui::address("local token", &format!("{local_token_addr}"));

    // Verify tokenId by calling interchainTokenId() on the deployed token
    let on_chain_id = InterchainToken::new(local_token_addr, &provider)
        .interchainTokenId()
        .call()
        .await?;
    if on_chain_id != token_id {
        return Err(eyre::eyre!(
            "tokenId mismatch: event={token_id} on-chain={on_chain_id}"
        ));
    }
    ui::success("tokenId verified on-chain");

    // ── Step 2: Deploy remote interchain token to flow ──────────────────
    ui::step_header(2, TOTAL_STEPS, "Deploy remote interchain token to flow");

    let gas_value = U256::from(2_000_000_000_000_000_000u64); // 2 ETH for cross-chain gas
    ui::kv("destination", DEST_CHAIN);
    ui::kv("gas value", &format!("{gas_value} wei"));

    let remote_call = factory
        .deployRemoteInterchainToken(salt, DEST_CHAIN.to_string(), gas_value)
        .value(gas_value);

    let pending = match remote_call.send().await {
        Ok(p) => p,
        Err(e) => {
            let err_debug = format!("{e:?}");
            if err_debug.contains("f9188a68") || err_debug.contains("UntrustedChain") {
                ui::error("UntrustedChain() — the destination chain is not trusted by this ITS");
                ui::info("Run `test its` again for detailed remediation steps.");
                return Ok(());
            }
            return Err(e.into());
        }
    };
    let tx_hash = *pending.tx_hash();
    let receipt = crate::evm::broadcast_and_log(pending, "tx").await?;

    // Extract ContractCall event to get message details
    let (event_index, payload, payload_hash, destination_chain, destination_address) =
        extract_contract_call_event(&receipt)?;

    let message_id = format!("{tx_hash:#x}-{event_index}");
    let source_address = format!("{its_proxy_addr}");

    ui::kv("message_id", &message_id);
    ui::kv("payload_hash", &format!("{payload_hash}"));
    ui::kv("destination_chain", &destination_chain);
    ui::kv("destination_address", &destination_address);
    ui::kv("source_address", &source_address);

    // ── Amplifier routing: source → hub ─────────────────────────────────
    ui::section("Amplifier Routing (source → hub)");

    let (signing_key, axelar_address) = derive_axelar_wallet(&state.mnemonic)?;
    let (lcd, chain_id, fee_denom, gas_price) = read_axelar_config(&target_json)?;

    let cosm_gateway = read_axelar_contract_field(
        &target_json,
        &format!("/axelar/contracts/Gateway/{axelar_id}/address"),
    )?;
    let voting_verifier = read_axelar_contract_field(
        &target_json,
        &format!("/axelar/contracts/VotingVerifier/{axelar_id}/address"),
    )?;

    ui::address("cosmos gateway", &cosm_gateway);
    ui::address("voting verifier", &voting_verifier);
    ui::address("axelar address", &axelar_address);

    let its_msg = json!({
        "cc_id": {
            "message_id": message_id,
            "source_chain": axelar_id,
        },
        "destination_chain": destination_chain,
        "destination_address": destination_address,
        "source_address": source_address,
        "payload_hash": alloy::hex::encode(payload_hash.as_slice()),
    });

    // ── Step 3: verify_messages ─────────────────────────────────────────
    ui::step_header(3, TOTAL_STEPS, "verify_messages");
    let poll_id = submit_verify_messages_amplifier(
        &its_msg,
        &signing_key,
        &axelar_address,
        &lcd,
        &chain_id,
        &fee_denom,
        gas_price,
        &cosm_gateway,
    )
    .await?;

    if let Some(poll_id) = poll_id {
        ui::kv("poll_id", &poll_id);

        // ── Step 4: Wait for poll votes + end poll ──────────────────────────
        ui::step_header(4, TOTAL_STEPS, "Wait for poll votes + end poll");
        wait_for_poll_votes(&lcd, &voting_verifier, &poll_id).await?;
        end_poll_with_retry(
            &poll_id,
            &signing_key,
            &axelar_address,
            &lcd,
            &chain_id,
            &fee_denom,
            gas_price,
            &voting_verifier,
        )
        .await?;
    } else {
        ui::info("no new poll created — message already being verified by active verifiers");
        ui::step_header(4, TOTAL_STEPS, "Wait for poll votes + end poll");
        ui::info("skipped (existing poll)");
    }

    // ── Step 5: route_messages ──────────────────────────────────────────
    ui::step_header(5, TOTAL_STEPS, "route_messages");
    route_messages_with_retry(
        &its_msg,
        &signing_key,
        &axelar_address,
        &lcd,
        &chain_id,
        &fee_denom,
        gas_price,
        &cosm_gateway,
    )
    .await?;

    // ── Step 6: Execute on AxelarnetGateway (hub) ───────────────────────
    ui::step_header(6, TOTAL_STEPS, "Execute on AxelarnetGateway (hub)");

    let axelarnet_gateway =
        read_axelar_contract_field(&target_json, "/axelar/contracts/AxelarnetGateway/address")?;
    ui::address("AxelarnetGateway", &axelarnet_gateway);

    execute_on_axelarnet_gateway(
        &message_id,
        &axelar_id,
        DEST_CHAIN,
        &payload,
        &signing_key,
        &axelar_address,
        &lcd,
        &chain_id,
        &fee_denom,
        gas_price,
        &axelarnet_gateway,
    )
    .await?;

    // ── Step 7: Poll destination chain to confirm token deployed ─────────
    let dest_provider = ProviderBuilder::new().connect_http(dest_rpc.parse()?);
    let predicted_addr =
        poll_for_remote_token_deploy(&dest_provider, dest_its_addr, token_id).await?;

    // ── Step 8: Send interchain transfer ────────────────────────────────
    ui::step_header(8, TOTAL_STEPS, "Send interchain transfer");

    let transfer_amount = U256::from(100u64)
        * U256::from(10u64).pow(U256::from(crate::types::EVM_LEGACY_SPEC.decimals));
    let receiver: Address = "0x000000000000000000000000000000000000dEaD".parse()?;
    let receiver_bytes = Bytes::copy_from_slice(receiver.as_slice());
    let transfer_gas = U256::from(200_000_000_000_000_000u64); // 0.2 ETH for gas

    ui::kv("amount", &format!("{transfer_amount}"));
    ui::address("receiver", &format!("{receiver}"));
    ui::kv("gas value", &format!("{transfer_gas} wei"));

    let local_token = InterchainToken::new(local_token_addr, &provider);
    let transfer_call = local_token
        .interchainTransfer(
            DEST_CHAIN.to_string(),
            receiver_bytes,
            transfer_amount,
            Bytes::new(), // empty metadata
        )
        .value(transfer_gas);

    let pending = transfer_call.send().await?;
    let tx_hash = *pending.tx_hash();
    let receipt = crate::evm::broadcast_and_log(pending, "tx").await?;

    // Extract ContractCall event for the transfer
    let (xfer_event_index, xfer_payload, xfer_payload_hash, xfer_dest_chain, xfer_dest_addr) =
        extract_contract_call_event(&receipt)?;

    let xfer_message_id = format!("{tx_hash:#x}-{xfer_event_index}");
    ui::kv("message_id", &xfer_message_id);
    ui::kv("destination_chain", &xfer_dest_chain);

    // ── Step 9: Relay transfer to hub ────────────────────────────────────
    ui::step_header(9, TOTAL_STEPS, "Relay transfer to hub");

    relay_to_hub(
        &axelar_id,
        &xfer_message_id,
        &source_address,
        &xfer_dest_chain,
        &xfer_dest_addr,
        &xfer_payload_hash,
        &xfer_payload,
        &signing_key,
        &axelar_address,
        &lcd,
        &chain_id,
        &fee_denom,
        gas_price,
        &cosm_gateway,
        &voting_verifier,
        &axelarnet_gateway,
    )
    .await?;

    // ── Step 10: Verify transfer on destination ──────────────────────────
    poll_for_balance_on_destination(&dest_provider, predicted_addr, receiver).await;

    // ── Complete ────────────────────────────────────────────────────────
    ui::section("Complete");
    ui::success(&format!(
        "ITS flow complete ({})",
        ui::format_elapsed(start)
    ));

    Ok(())
}

/// Generate a random salt using timestamp to avoid collisions.
pub fn generate_salt() -> FixedBytes<32> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let encoded = ("its-test", U256::from(nanos)).abi_encode_params();
    keccak256(&encoded)
}

/// Relay a message through the Amplifier pipeline: verify → poll → route → execute on hub.
#[allow(clippy::too_many_arguments)]
async fn relay_to_hub(
    axelar_id: &str,
    message_id: &str,
    source_address: &str,
    destination_chain: &str,
    destination_address: &str,
    payload_hash: &FixedBytes<32>,
    payload: &[u8],
    signing_key: &cosmrs::crypto::secp256k1::SigningKey,
    axelar_address: &str,
    lcd: &str,
    chain_id: &str,
    fee_denom: &str,
    gas_price: f64,
    cosm_gateway: &str,
    voting_verifier: &str,
    axelarnet_gateway: &str,
) -> Result<()> {
    let msg = json!({
        "cc_id": {
            "message_id": message_id,
            "source_chain": axelar_id,
        },
        "destination_chain": destination_chain,
        "destination_address": destination_address,
        "source_address": source_address,
        "payload_hash": alloy::hex::encode(payload_hash.as_slice()),
    });

    ui::info("verify_messages...");
    let poll_id = submit_verify_messages_amplifier(
        &msg,
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        cosm_gateway,
    )
    .await?;

    if let Some(poll_id) = poll_id {
        ui::kv("poll_id", &poll_id);
        wait_for_poll_votes(lcd, voting_verifier, &poll_id).await?;
        end_poll_with_retry(
            &poll_id,
            signing_key,
            axelar_address,
            lcd,
            chain_id,
            fee_denom,
            gas_price,
            voting_verifier,
        )
        .await?;
    } else {
        ui::info("no new poll — already being verified by active verifiers");
    }

    ui::info("route_messages...");
    route_messages_with_retry(
        &msg,
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        cosm_gateway,
    )
    .await?;

    ui::info("execute on AxelarnetGateway...");
    execute_on_axelarnet_gateway(
        message_id,
        axelar_id,
        destination_chain,
        payload,
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        axelarnet_gateway,
    )
    .await?;

    Ok(())
}

/// Print the cast-send remediation block when DEST_CHAIN isn't trusted on the
/// source-chain ITS (or vice versa). The owner addresses are queried so the
/// user knows which key needs to sign the setTrustedChain calls.
async fn print_untrusted_chain_remediation<P: Provider>(
    axelar_id: &str,
    its_proxy_addr: Address,
    dest_its_addr: Address,
    rpc_url: &str,
    dest_rpc: &str,
    provider: &P,
) -> Result<()> {
    ui::error(&format!(
        "\"{DEST_CHAIN}\" is not a trusted chain on the ITS at {its_proxy_addr}"
    ));

    let source_owner = Ownable::new(its_proxy_addr, provider)
        .owner()
        .call()
        .await
        .ok();

    let dest_provider = ProviderBuilder::new().connect_http(dest_rpc.parse()?);
    let flow_owner = Ownable::new(dest_its_addr, &dest_provider)
        .owner()
        .call()
        .await
        .ok();

    let mut lines: Vec<String> = vec![
        format!("The ITS on {axelar_id} does not trust \"{DEST_CHAIN}\" as a destination chain."),
        String::new(),
        format!("1. On {axelar_id} — set \"{DEST_CHAIN}\" as trusted:"),
    ];
    if let Some(owner) = source_owner {
        lines.push(format!("   owner: {owner}"));
    }
    lines.push(format!("   cast send {its_proxy_addr} \\"));
    lines.push("     'setTrustedChain(string)' \\".to_string());
    lines.push(format!("     '{DEST_CHAIN}' \\"));
    lines.push(format!("     --rpc-url {rpc_url} \\"));
    lines.push("     --private-key $PRIVATE_KEY".into());
    lines.push(String::new());
    lines.push(format!(
        "2. On {DEST_CHAIN} — set \"{axelar_id}\" as trusted:"
    ));
    if let Some(owner) = flow_owner {
        lines.push(format!("   owner: {owner}"));
    }
    lines.push(format!("   cast send {dest_its_addr} \\"));
    lines.push("     'setTrustedChain(string)' \\".to_string());
    lines.push(format!("     '{axelar_id}' \\"));
    lines.push(format!("     --rpc-url {dest_rpc} \\"));
    lines.push("     --private-key $PRIVATE_KEY".into());
    lines.push(String::new());
    lines.push("Both sides must trust each other for cross-chain ITS to work.".into());

    let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
    ui::action_required(&line_refs);
    Ok(())
}

/// Wait for the destination-chain ITS to deploy the predicted token contract
/// (post hub relay). Uses `name()` instead of `get_code_at` since the latter
/// is unreliable on some EVMs (Flow). Returns the predicted address either
/// way; the caller can decide what to do if name() never responds.
async fn poll_for_remote_token_deploy<P: Provider>(
    dest_provider: &P,
    dest_its_addr: Address,
    token_id: FixedBytes<32>,
) -> Result<Address> {
    let dest_its = InterchainTokenService::new(dest_its_addr, dest_provider);

    ui::step_header(
        7,
        TOTAL_STEPS,
        &format!("Poll {DEST_CHAIN} for token deployment"),
    );
    ui::address(&format!("{DEST_CHAIN} ITS"), &format!("{dest_its_addr}"));
    ui::kv("tokenId", &format!("{token_id}"));

    let predicted_addr = dest_its
        .interchainTokenAddress(token_id)
        .call()
        .await
        .map_err(|e| eyre::eyre!("failed to query interchainTokenAddress on {DEST_CHAIN}: {e}"))?;
    ui::address("predicted token addr", &format!("{predicted_addr}"));

    let spinner = ui::wait_spinner(&format!("Waiting for token to appear on {DEST_CHAIN}..."));
    let mut deployed = false;

    for i in 0..DEST_CHAIN_POLL_ATTEMPTS {
        if i > 0 {
            tokio::time::sleep(DEST_CHAIN_POLL_INTERVAL).await;
        }
        let token = ERC20::new(predicted_addr, dest_provider);
        match token.name().call().await {
            Ok(name) => {
                spinner.finish_and_clear();
                ui::success(&format!("Token responds to name() → \"{name}\""));
                deployed = true;
                break;
            }
            Err(_) => {
                spinner.set_message(format!(
                    "Token not yet deployed (attempt {}/30, addr={predicted_addr})...",
                    i + 1
                ));
            }
        }
    }
    spinner.finish_and_clear();

    if deployed {
        ui::success(&format!("Token deployed on {DEST_CHAIN}!"));
        ui::address(
            &format!("token address ({DEST_CHAIN})"),
            &format!("{predicted_addr}"),
        );
    } else {
        ui::warn(&format!(
            "Token not yet deployed on {DEST_CHAIN} after 5 minutes"
        ));
        ui::info("The relayer may still be processing. Check axelarscan for status.");
        ui::kv("tokenId", &format!("{token_id}"));
    }

    Ok(predicted_addr)
}

/// Poll the destination-chain ERC20 until the receiver's balance is non-zero
/// (i.e. the relayer has executed the transfer). Logs success/timeout but
/// never errors — a stuck relay isn't a fatal test failure.
async fn poll_for_balance_on_destination<P: Provider>(
    dest_provider: &P,
    predicted_addr: Address,
    receiver: Address,
) {
    ui::step_header(10, TOTAL_STEPS, &format!("Verify transfer on {DEST_CHAIN}"));
    ui::address("token", &format!("{predicted_addr}"));
    ui::address("receiver", &format!("{receiver}"));

    let dest_token = ERC20::new(predicted_addr, dest_provider);
    let spinner = ui::wait_spinner(&format!("Waiting for balance to appear on {DEST_CHAIN}..."));

    let mut final_balance = U256::ZERO;
    for i in 0..DEST_CHAIN_POLL_ATTEMPTS {
        if i > 0 {
            tokio::time::sleep(DEST_CHAIN_POLL_INTERVAL).await;
        }
        match dest_token.balanceOf(receiver).call().await {
            Ok(bal) => {
                if bal > U256::ZERO {
                    final_balance = bal;
                    break;
                }
                spinner.set_message(format!("Balance still 0 (attempt {}/30)...", i + 1));
            }
            Err(_) => {
                spinner.set_message(format!("Query failed (attempt {}/30)...", i + 1));
            }
        }
    }
    spinner.finish_and_clear();

    if final_balance > U256::ZERO {
        ui::success(&format!(
            "Receiver {receiver} has balance {final_balance} on {DEST_CHAIN}"
        ));
    } else {
        ui::warn(&format!("Balance still 0 on {DEST_CHAIN} after 5 minutes"));
        ui::info("The relayer may still be processing. Check axelarscan for status.");
    }
}

/// Extract tokenId and token address from InterchainTokenDeployed event in receipt logs.
/// Reads topics/data directly to avoid ABI decode issues with indexed field differences.
pub fn extract_token_deployed_event(
    receipt: &alloy::rpc::types::TransactionReceipt,
) -> Result<(FixedBytes<32>, Address)> {
    for log in receipt.inner.logs() {
        if log.topics().first() == Some(&InterchainTokenDeployed::SIGNATURE_HASH) {
            // tokenId is always topics[1] (first indexed param)
            let token_id = *log
                .topics()
                .get(1)
                .ok_or_else(|| eyre::eyre!("InterchainTokenDeployed missing tokenId topic"))?;

            // tokenAddress is always the first ABI-encoded field in data (bytes 12..32)
            let data = log.data().data.as_ref();
            if data.len() >= 32 {
                let token_address = Address::from_slice(&data[12..32]);
                return Ok((token_id, token_address));
            }

            return Ok((token_id, Address::ZERO));
        }
    }

    Err(eyre::eyre!(
        "InterchainTokenDeployed event not found in receipt logs"
    ))
}

/// Extract ContractCall event data from a transaction receipt.
/// Returns (event_index, payload, payload_hash, destination_chain, destination_address).
pub fn extract_contract_call_event(
    receipt: &alloy::rpc::types::TransactionReceipt,
) -> Result<(usize, Vec<u8>, FixedBytes<32>, String, String)> {
    for (i, log) in receipt.inner.logs().iter().enumerate() {
        if log.topics().first() == Some(&ContractCall::SIGNATURE_HASH) {
            // Decode the event data (non-indexed fields)
            let decoded = ContractCall::decode_log(&log.inner)
                .map_err(|e| eyre::eyre!("failed to decode ContractCall event: {e}"))?;

            let payload_hash = decoded.topics().2; // payloadHash is the 3rd topic
            let destination_chain = decoded.data.destinationChain;
            let destination_address = decoded.data.destinationContractAddress;
            let payload = decoded.data.payload.to_vec();

            return Ok((
                i,
                payload,
                payload_hash,
                destination_chain,
                destination_address,
            ));
        }
    }

    Err(eyre::eyre!("ContractCall event not found in receipt logs"))
}

// ---------------------------------------------------------------------------
// Config-based ITS test (Solana → EVM with manual relay through ITS hub)
// ---------------------------------------------------------------------------

const PHASE_A_STEPS: usize = 11;
const PHASE_B_STEPS: usize = 9;

// Initial supply for the config-mode test, in base units of `ITS_CONFIG_SPEC`.
// 1_000_000_000_000 = 1000 tokens at 9 decimals.
const INITIAL_SUPPLY: u64 = 1_000_000_000_000;

// ITS message-type discriminators are the `ItsMessageType` enum in `types.rs`.

// Cache files are namespaced by `Network::from_features()` so a `mainnet`
// build doesn't read a `testnet` deploy from disk.

/// Cache of a successful Phase A run. Keyed on
/// `(network, src, dst, deployer_pubkey)` so a fresh run can skip the deploy
/// and go straight to Phase B if the previously-deployed token still exists.
#[derive(serde::Serialize, serde::Deserialize)]
struct ItsTestCache {
    deployer: String,
    salt_hex: String,
    token_id_hex: String,
    dest_token_address: String,
}

fn cache_path(src: &str, dst: &str, deployer: &str) -> PathBuf {
    let data_dir = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("axe");
    data_dir.join(format!(
        "its-test-{}-{src}-{dst}-{deployer}.json",
        crate::types::Network::from_features()
    ))
}

fn read_cache(path: &Path) -> Option<ItsTestCache> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn save_cache(path: &Path, cache: &ItsTestCache) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(cache)?)?;
    Ok(())
}

/// Probe whether the destination ITS trusts the source chain. Two ITS API
/// generations are deployed in the wild — the older one returns a string from
/// `trustedAddress(chain)`, the newer exposes `isTrustedChain(chain)`. If
/// neither says trusted, print remediation and return `Ok(false)`.
async fn check_destination_trusts_source<P: Provider>(
    its: &InterchainTokenService::InterchainTokenServiceInstance<&P>,
    src_axelar_id: &crate::types::ChainAxelarId,
    dst_its_proxy: Address,
    dst: &str,
    dst_rpc: &str,
) -> Result<bool> {
    let legacy_trust = its
        .trustedAddress(src_axelar_id.clone().into())
        .call()
        .await
        .ok();
    let new_trust = its
        .isTrustedChain(src_axelar_id.clone().into())
        .call()
        .await
        .ok();
    let trusted = match (legacy_trust.as_deref(), new_trust) {
        (Some(s), _) if !s.is_empty() => true,
        (_, Some(b)) => b,
        _ => false,
    };
    if trusted {
        return Ok(true);
    }

    ui::error(&format!(
        "destination ITS at {dst_its_proxy} on {dst} does not trust source chain '{src_axelar_id}'"
    ));
    let owner = Ownable::new(dst_its_proxy, its.provider())
        .owner()
        .call()
        .await
        .ok();
    let mut lines: Vec<String> = vec![format!(
        "Set '{src_axelar_id}' as trusted on the destination ITS:"
    )];
    if let Some(owner) = owner {
        lines.push(format!("  owner: {owner}"));
    }
    lines.push(format!(
        "  cast send {dst_its_proxy} 'setTrustedChain(string)' '{src_axelar_id}' \\"
    ));
    lines.push(format!(
        "    --rpc-url {dst_rpc} --private-key $PRIVATE_KEY"
    ));
    let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
    ui::action_required(&line_refs);
    Ok(false)
}

/// Resolve the hub bech32 the destination ITS expects in `execute`'s
/// `sourceAddress`. Probes legacy `trustedAddress("axelar")` first, then new
/// `itsHubAddress()`, falling back to the cosm config.
async fn resolve_hub_address_evm_view<P: Provider>(
    its: &InterchainTokenService::InterchainTokenServiceInstance<&P>,
    its_hub_address: &str,
) -> String {
    match its
        .trustedAddress(crate::types::HubChain::NAME.to_string())
        .call()
        .await
    {
        Ok(s) if !s.is_empty() => s,
        _ => match its.itsHubAddress().call().await {
            Ok(s) if !s.is_empty() => s,
            _ => its_hub_address.to_string(),
        },
    }
}

/// If a cached Phase-A deploy for this (src, dst, deployer) tuple still has a
/// valid destination token (responds to `name()`), return it so we can skip
/// Phase A entirely. Returns None on any cache miss / staleness.
async fn try_load_cached_phase_a<P: Provider>(
    cache_file: &Path,
    fresh_token: bool,
    sol_pubkey: &solana_sdk::pubkey::Pubkey,
    dst_provider: &P,
) -> Option<(String, [u8; 32], Address)> {
    if fresh_token {
        return None;
    }
    let c = read_cache(cache_file)?;
    if c.deployer != sol_pubkey.to_string() {
        return None;
    }
    let tid_bytes: [u8; 32] = match alloy::hex::decode(c.token_id_hex.trim_start_matches("0x")) {
        Ok(b) if b.len() == 32 => {
            let mut a = [0u8; 32];
            a.copy_from_slice(&b);
            a
        }
        _ => return None,
    };
    let addr: Address = c.dest_token_address.parse().ok()?;
    let token = ERC20::new(addr, dst_provider);
    match token.name().call().await {
        Ok(name) => Some((name, tid_bytes, addr)),
        Err(_) => None,
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run_config(
    config: PathBuf,
    source_chain: Option<String>,
    destination_chain: Option<String>,
    mnemonic_override: Option<String>,
    evm_private_key_override: Option<String>,
    amount: Option<u64>,
    gas_value: Option<u64>,
    fresh_token: bool,
) -> Result<()> {
    let start = Instant::now();
    // `amount` is consumed in Phase B (interchain transfer); silence the unused
    // binding here without dropping the CLI flag plumbing.
    let _ = amount;
    let gas_value = gas_value.unwrap_or(10_000_000);

    let config_content =
        std::fs::read_to_string(&config).map_err(|e| eyre::eyre!("failed to read config: {e}"))?;
    let config_root: serde_json::Value = serde_json::from_str(&config_content)?;

    let chains = config_root
        .get("chains")
        .and_then(|v| v.as_object())
        .ok_or_else(|| eyre::eyre!("no 'chains' in config"))?;

    let src = source_chain.ok_or_else(|| eyre::eyre!("--source-chain required"))?;
    let dst = destination_chain.ok_or_else(|| eyre::eyre!("--destination-chain required"))?;

    let src_entry = chains
        .get(&src)
        .ok_or_else(|| eyre::eyre!("source chain '{src}' not found in config"))?;
    let dst_entry = chains
        .get(&dst)
        .ok_or_else(|| eyre::eyre!("destination chain '{dst}' not found in config"))?;

    use crate::types::ChainType;
    let src_type: ChainType = src_entry
        .get("chainType")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("source chain '{src}' has no chainType"))?
        .parse()?;
    let dst_type: ChainType = dst_entry
        .get("chainType")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("destination chain '{dst}' has no chainType"))?
        .parse()?;

    if src_type != ChainType::Svm || dst_type != ChainType::Evm {
        return Err(eyre::eyre!(
            "ITS config-mode currently supports svm → evm only (got {src_type} → {dst_type})"
        ));
    }

    let src_rpc = src_entry
        .get("rpc")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("no RPC for source chain '{src}'"))?
        .to_string();
    let dst_rpc = dst_entry
        .get("rpc")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("no RPC for destination chain '{dst}'"))?
        .to_string();

    // Cosmos-side identifiers for the source/destination chains. Consensus
    // chains use a capitalised axelarId distinct from the JSON key — keep
    // them as separate types so the compiler refuses to confuse them.
    use crate::types::ChainAxelarId;
    let src_axelar_id: ChainAxelarId = src_entry
        .get("axelarId")
        .and_then(|v| v.as_str())
        .unwrap_or(&src)
        .to_owned()
        .into();
    let dst_axelar_id: ChainAxelarId = dst_entry
        .get("axelarId")
        .and_then(|v| v.as_str())
        .unwrap_or(&dst)
        .to_owned()
        .into();

    ui::section(&format!("ITS Test: {src} → {dst}"));
    ui::kv("source", &format!("{src} ({src_axelar_id}, {src_type})"));
    ui::kv(
        "destination",
        &format!("{dst} ({dst_axelar_id}, {dst_type})"),
    );

    // --- Preflight: derive Axelar wallet, fund checks ---
    let mnemonic = mnemonic_override
        .or_else(|| std::env::var("MNEMONIC").ok())
        .ok_or_else(|| eyre::eyre!("MNEMONIC env var or --mnemonic required for relay"))?;
    let (signing_key, axelar_address) = derive_axelar_wallet(&mnemonic)?;
    let (lcd, chain_id, fee_denom, gas_price) = read_axelar_config(&config)?;
    let axelar_rpc = read_axelar_rpc(&config)?;

    ui::section("Preflight");
    ui::address("axelar address", &axelar_address);
    // ITS does ~7-8 cosmos txs per phase across 2 phases; bump min from GMP's 100k.
    const MIN_RELAY_BALANCE_UAXL: u128 = 200_000;
    crate::cosmos::check_axelar_balance(
        &lcd,
        &chain_id,
        &axelar_address,
        &fee_denom,
        MIN_RELAY_BALANCE_UAXL,
    )
    .await?;

    let sol_keypair = crate::solana::load_keypair(None)?;
    let sol_pubkey = sol_keypair.pubkey();
    crate::solana::check_solana_balance(
        &src_rpc,
        "source",
        &sol_pubkey,
        crate::solana::MIN_SOL_ITS_LAMPORTS,
    )?;

    // EVM signer is only used to derive the receiver address (we never send EVM
    // txs from this key — that's done with deployer / cosm-mnemonic-derived flow).
    let evm_pk = evm_private_key_override
        .or_else(|| std::env::var("EVM_PRIVATE_KEY").ok())
        .ok_or_else(|| {
            eyre::eyre!("EVM_PRIVATE_KEY env var or --evm-private-key required (used to sign destination EVM txs)")
        })?;
    let evm_signer: PrivateKeySigner = evm_pk.parse()?;
    let evm_signer_address = evm_signer.address();
    ui::address("evm signer / receiver", &format!("{evm_signer_address}"));

    let token_symbol = dst_entry
        .get("tokenSymbol")
        .and_then(|v| v.as_str())
        .unwrap_or("ETH");
    preflight::check_evm_balances(
        &dst_rpc,
        &[("dest evm signer", evm_signer_address)],
        token_symbol,
    )
    .await?;

    let dst_provider = ProviderBuilder::new()
        .wallet(evm_signer.clone())
        .connect_http(dst_rpc.parse()?);

    // --- Resolve contract addresses ---
    use crate::types::{cosm_gateway_pointer, multisig_prover_pointer, voting_verifier_pointer};
    let dst_its_proxy = read_contract_address(&config, &dst, "InterchainTokenService")?;
    let dst_evm_gateway = read_contract_address(&config, &dst, "AxelarGateway")?;
    let src_cosm_gateway =
        read_axelar_contract_field(&config, &cosm_gateway_pointer(&src_axelar_id))?;
    let voting_verifier =
        read_axelar_contract_field(&config, &voting_verifier_pointer(&src_axelar_id))?;
    let dst_cosm_gateway =
        read_axelar_contract_field(&config, &cosm_gateway_pointer(&dst_axelar_id))?;
    let dst_multisig_prover =
        read_axelar_contract_field(&config, &multisig_prover_pointer(&dst_axelar_id))?;
    let axelarnet_gateway =
        read_axelar_contract_field(&config, "/axelar/contracts/AxelarnetGateway/address")?;
    let its_hub_address =
        read_axelar_contract_field(&config, "/axelar/contracts/InterchainTokenService/address")?;

    ui::address("dest ITS proxy", &format!("{dst_its_proxy}"));
    ui::address("dest EVM gateway", &format!("{dst_evm_gateway}"));
    ui::address("source cosm gateway", &src_cosm_gateway);
    ui::address("dest cosm gateway", &dst_cosm_gateway);
    ui::address("multisig prover (dst)", &dst_multisig_prover);
    ui::address("AxelarnetGateway", &axelarnet_gateway);
    ui::address("ITS hub (cosm)", &its_hub_address);

    // --- Trust-chain check: dest ITS must trust the source chain ---
    let its = InterchainTokenService::new(dst_its_proxy, &dst_provider);
    if !check_destination_trusts_source(&its, &src_axelar_id, dst_its_proxy, &dst, &dst_rpc).await?
    {
        return Ok(());
    }
    ui::success(&format!("destination ITS trusts '{src_axelar_id}'"));

    let hub_address_evm_view = resolve_hub_address_evm_view(&its, &its_hub_address).await;
    ui::kv("hub address (EVM view)", &hub_address_evm_view);

    // ─────────────────────────────────────────────────────────────────────
    // Phase A: deploy interchain token (local + remote with manual relay)
    //
    // Idempotent: if a previous Phase A run for the same
    // (network, src, dst, deployer) is cached on disk and the destination
    // token still responds to `name()`, skip the deploy entirely and reuse
    // the cached tokenId. Pass `--fresh-token` to force a redeploy.
    // ─────────────────────────────────────────────────────────────────────
    let cache_file = cache_path(&src, &dst, &sol_pubkey.to_string());
    let cached =
        try_load_cached_phase_a(&cache_file, fresh_token, &sol_pubkey, &dst_provider).await;

    let (token_id, dest_token_addr) = if let Some((name, tid, addr)) = cached {
        ui::section("Phase A: skipped (cached deploy still valid)");
        ui::kv("cache file", &cache_file.display().to_string());
        ui::kv("tokenId", &format!("0x{}", alloy::hex::encode(tid)));
        ui::address("dest token address", &format!("{addr}"));
        ui::success(&format!("dest token responds to name() → \"{name}\""));
        (tid, addr)
    } else {
        run_phase_a_deploy(
            &src,
            &dst,
            &src_axelar_id,
            &dst_axelar_id,
            &src_rpc,
            &sol_keypair,
            sol_pubkey,
            &signing_key,
            &axelar_address,
            &lcd,
            &chain_id,
            &fee_denom,
            gas_price,
            &src_cosm_gateway,
            &voting_verifier,
            &axelarnet_gateway,
            &dst_cosm_gateway,
            &dst_multisig_prover,
            &axelar_rpc,
            &its_hub_address,
            dst_its_proxy,
            dst_evm_gateway,
            &dst_provider,
            &its,
            gas_value,
            &cache_file,
            start,
        )
        .await?
    };

    // ─────────────────────────────────────────────────────────────────────
    // Phase B: interchain transfer (manual relay)
    // ─────────────────────────────────────────────────────────────────────
    let phase_b_start = Instant::now();
    let amount = amount.unwrap_or(1_000_000); // 0.001 token at 9 decimals
    let receiver_bytes = evm_signer_address.as_slice().to_vec();
    let token_program_2022 =
        solana_sdk::pubkey::Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
    let ata_program =
        solana_sdk::pubkey::Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
    let (its_root_pda, _) = crate::solana::find_its_root_pda();
    let (mint, _) = crate::solana::find_interchain_token_pda(&its_root_pda, &token_id);
    let source_ata = solana_sdk::pubkey::Pubkey::find_program_address(
        &[
            sol_pubkey.as_ref(),
            token_program_2022.as_ref(),
            mint.as_ref(),
        ],
        &ata_program,
    )
    .0;

    ui::section("Phase B: interchain transfer (manual relay)");
    ui::address("mint", &mint.to_string());
    ui::address("source ATA", &source_ata.to_string());
    ui::kv("amount (base units)", &format!("{amount}"));
    ui::address("receiver (EVM)", &format!("{evm_signer_address}"));

    // Capture the destination ERC20 balance BEFORE the transfer so we can
    // verify a strict delta later.
    let erc20 = ERC20::new(dest_token_addr, &dst_provider);
    let pre_balance = erc20
        .balanceOf(evm_signer_address)
        .call()
        .await
        .unwrap_or(U256::ZERO);
    ui::kv("pre-transfer balance", &format!("{pre_balance}"));

    // Step B1: Solana — fire the InterchainTransfer
    ui::step_header(1, PHASE_B_STEPS, "Send InterchainTransfer (Solana → hub)");
    let (xfer_sig, _metrics) = crate::solana::send_its_interchain_transfer(
        &src_rpc,
        &sol_keypair,
        &token_id,
        &source_ata,
        &mint,
        dst_axelar_id.as_str(),
        &receiver_bytes,
        amount,
        gas_value,
    )?;
    ui::tx_hash("solana tx", &xfer_sig);

    let xfer_first_leg_id = crate::solana::extract_its_message_id(&src_rpc, &xfer_sig)?;
    ui::kv("first-leg message_id", &xfer_first_leg_id);

    let xfer_gw = crate::solana::extract_gateway_call_contract_payload(&src_rpc, &xfer_sig)?;
    ui::kv("gateway sender", &xfer_gw.sender);
    ui::kv("gateway destination_chain", &xfer_gw.destination_chain);
    ui::kv("gateway destination_address", &xfer_gw.destination_address);
    ui::kv(
        "gateway payload_hash",
        &format!("0x{}", alloy::hex::encode(xfer_gw.payload_hash)),
    );

    // Step B2: drive source → hub
    ui::step_header(
        2,
        PHASE_B_STEPS,
        "Source → hub (verify, route, hub-execute)",
    );
    let xfer_payload_hash = FixedBytes::<32>::from(xfer_gw.payload_hash);
    relay_to_hub(
        src_axelar_id.as_str(),
        &xfer_first_leg_id,
        &xfer_gw.sender,
        crate::types::HubChain::NAME,
        &its_hub_address,
        &xfer_payload_hash,
        &xfer_gw.payload,
        &signing_key,
        &axelar_address,
        &lcd,
        &chain_id,
        &fee_denom,
        gas_price,
        &src_cosm_gateway,
        &voting_verifier,
        &axelarnet_gateway,
    )
    .await?;

    // Step B3+: hub → destination (reconstruct RECEIVE_FROM_HUB envelope and drive proof + execute)
    let xfer_inner = encode_inner_transfer(
        &token_id,
        sol_pubkey.to_bytes().as_slice(),
        &receiver_bytes,
        amount,
        &[],
    );
    let xfer_dest_payload = encode_receive_from_hub(&src_axelar_id, &xfer_inner);

    let _xfer_command_id = relay_to_destination(
        &xfer_first_leg_id,
        &src_axelar_id,
        &xfer_dest_payload,
        &dst_axelar_id,
        &dst,
        dst_its_proxy,
        dst_evm_gateway,
        &dst_provider,
        &signing_key,
        &axelar_address,
        &lcd,
        &chain_id,
        &fee_denom,
        gas_price,
        &dst_cosm_gateway,
        &dst_multisig_prover,
        &axelarnet_gateway,
        &axelar_rpc,
        3, // step base for Phase B's ui (B3..B8)
        PHASE_B_STEPS,
    )
    .await?;

    // Step B-final: verify ERC20 balance went up by exactly `amount`.
    ui::step_header(
        PHASE_B_STEPS,
        PHASE_B_STEPS,
        "Verify ERC20 balance on destination",
    );
    let post_balance = erc20.balanceOf(evm_signer_address).call().await?;
    let delta = post_balance.saturating_sub(pre_balance);
    ui::kv("post-transfer balance", &format!("{post_balance}"));
    ui::kv("delta", &format!("{delta}"));
    if delta == U256::from(amount) {
        ui::success(&format!(
            "receiver balance increased by exactly {amount} base units"
        ));
    } else {
        return Err(eyre::eyre!(
            "balance delta {delta} does not match expected {amount} (post={post_balance}, pre={pre_balance})"
        ));
    }

    ui::section("Phase B complete");
    ui::success(&format!(
        "transfer + manual relay finished ({})",
        ui::format_elapsed(phase_b_start)
    ));

    ui::section("All phases complete");
    ui::success(&format!("total elapsed: {}", ui::format_elapsed(start)));

    Ok(())
}

// ---------------------------------------------------------------------------
// Phase A driver (deploy local + remote, manual relay both legs)
// ---------------------------------------------------------------------------

/// Returns `(token_id, dest_token_addr)` on success and writes the result to
/// `cache_file` so a subsequent run skips this entire phase.
#[allow(clippy::too_many_arguments)]
async fn run_phase_a_deploy<P: Provider>(
    src: &str,
    dst: &str,
    src_axelar_id: &crate::types::ChainAxelarId,
    dst_axelar_id: &crate::types::ChainAxelarId,
    src_rpc: &str,
    sol_keypair: &solana_sdk::signature::Keypair,
    sol_pubkey: solana_sdk::pubkey::Pubkey,
    signing_key: &cosmrs::crypto::secp256k1::SigningKey,
    axelar_address: &str,
    lcd: &str,
    chain_id: &str,
    fee_denom: &str,
    gas_price: f64,
    src_cosm_gateway: &str,
    voting_verifier: &str,
    axelarnet_gateway: &str,
    dst_cosm_gateway: &str,
    dst_multisig_prover: &str,
    axelar_rpc: &str,
    its_hub_address: &str,
    dst_its_proxy: Address,
    dst_evm_gateway: Address,
    dst_provider: &P,
    its: &InterchainTokenService::InterchainTokenServiceInstance<&P>,
    gas_value: u64,
    cache_file: &Path,
    phase_start: Instant,
) -> Result<([u8; 32], Address)> {
    let _ = src;
    let _ = dst;

    ui::section("Phase A: deploy local + remote (manual relay)");

    // Step A1: generate salt, derive token id
    let salt = generate_salt();
    let salt_bytes: [u8; 32] = salt.0;
    let token_id = crate::solana::interchain_token_id(&sol_pubkey, &salt_bytes);
    let token_id_b32 = FixedBytes::<32>::from(token_id);

    ui::step_header(1, PHASE_A_STEPS, "Generate salt + tokenId");
    ui::kv("salt", &format!("0x{}", alloy::hex::encode(salt_bytes)));
    ui::kv("tokenId", &format!("0x{}", alloy::hex::encode(token_id)));

    // Step A2: Solana — deploy local interchain token
    ui::step_header(2, PHASE_A_STEPS, "Deploy local interchain token (Solana)");
    let spec = crate::types::ITS_CONFIG_SPEC;
    let local_sig = crate::solana::send_its_deploy_interchain_token(
        src_rpc,
        sol_keypair,
        &salt_bytes,
        spec.name,
        spec.symbol,
        spec.decimals,
        INITIAL_SUPPLY,
        None,
    )?;
    ui::tx_hash("solana tx", &local_sig);
    ui::success(&format!(
        "local mint deployed (initial supply {INITIAL_SUPPLY})"
    ));

    // Step A3: Solana — deploy remote interchain token (fires GMP)
    ui::step_header(
        3,
        PHASE_A_STEPS,
        "Deploy remote interchain token (Solana → hub)",
    );
    let remote_sig = crate::solana::send_its_deploy_remote_interchain_token(
        src_rpc,
        sol_keypair,
        &salt_bytes,
        dst_axelar_id.as_str(),
        gas_value,
    )?;
    ui::tx_hash("solana tx", &remote_sig);

    let first_leg_id = crate::solana::extract_its_message_id(src_rpc, &remote_sig)?;
    ui::kv("first-leg message_id", &first_leg_id);

    // Read the actual on-chain CallContractEvent. The verifiers will look up
    // the same fields; using on-chain values eliminates encoding-mismatch risk.
    let gw = crate::solana::extract_gateway_call_contract_payload(src_rpc, &remote_sig)?;
    ui::kv("gateway sender", &gw.sender);
    ui::kv("gateway destination_chain", &gw.destination_chain);
    ui::kv("gateway destination_address", &gw.destination_address);
    ui::kv(
        "gateway payload_hash",
        &format!("0x{}", alloy::hex::encode(gw.payload_hash)),
    );
    ui::kv(
        "gateway payload (len)",
        &format!("{} bytes", gw.payload.len()),
    );

    // Sanity: the local reconstruction should match what the gateway actually saw.
    let local_payload = encode_send_to_hub_deploy(
        dst_axelar_id.as_str(),
        &token_id,
        spec.name,
        spec.symbol,
        spec.decimals,
        None,
    )?;
    let local_hash = keccak256(&local_payload);
    if local_hash.as_slice() != gw.payload_hash {
        ui::warn("local payload reconstruction does not match on-chain payload:");
        ui::warn(&format!(
            "  local  : 0x{}",
            alloy::hex::encode(local_hash.as_slice())
        ));
        ui::warn(&format!(
            "  on-chain: 0x{}",
            alloy::hex::encode(gw.payload_hash)
        ));
    }

    let first_leg_payload = gw.payload.clone();
    let first_leg_payload_hash = FixedBytes::<32>::from(gw.payload_hash);
    let gw_sender = gw.sender.clone();

    // Step A4: drive source → hub via existing relay_to_hub helper
    ui::step_header(
        4,
        PHASE_A_STEPS,
        "Source → hub (verify, route, hub-execute)",
    );
    relay_to_hub(
        src_axelar_id.as_str(),
        &first_leg_id,
        &gw_sender,
        crate::types::HubChain::NAME,
        its_hub_address,
        &first_leg_payload_hash,
        &first_leg_payload,
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        src_cosm_gateway,
        voting_verifier,
        axelarnet_gateway,
    )
    .await?;

    // Step A5..10: hub → destination EVM, manual proof + execute
    let deploy_inner = encode_inner_deploy(&token_id, spec.name, spec.symbol, spec.decimals, &[]);
    let dest_payload_deploy = encode_receive_from_hub(src_axelar_id, &deploy_inner);

    let _command_id = relay_to_destination(
        &first_leg_id,
        src_axelar_id,
        &dest_payload_deploy,
        dst_axelar_id,
        dst,
        dst_its_proxy,
        dst_evm_gateway,
        dst_provider,
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        dst_cosm_gateway,
        dst_multisig_prover,
        axelarnet_gateway,
        axelar_rpc,
        5, // step base for ui
        PHASE_A_STEPS,
    )
    .await?;

    // Step A11: verify destination token is deployed
    ui::step_header(11, PHASE_A_STEPS, "Verify destination token deployed");
    let dest_token_addr = its.interchainTokenAddress(token_id_b32).call().await?;
    ui::address("dest token address", &format!("{dest_token_addr}"));
    let token = ERC20::new(dest_token_addr, dst_provider);
    match token.name().call().await {
        Ok(name) => {
            ui::success(&format!("dest token responds to name() → \"{name}\""));
        }
        Err(e) => {
            ui::warn(&format!("dest token name() failed: {e}"));
            ui::info("token may still be propagating — try again or check explorer");
        }
    }

    // Persist for next run.
    let cache = ItsTestCache {
        deployer: sol_pubkey.to_string(),
        salt_hex: format!("0x{}", alloy::hex::encode(salt_bytes)),
        token_id_hex: format!("0x{}", alloy::hex::encode(token_id)),
        dest_token_address: format!("{dest_token_addr}"),
    };
    if let Err(e) = save_cache(cache_file, &cache) {
        ui::warn(&format!(
            "failed to write cache to {}: {e}",
            cache_file.display()
        ));
    } else {
        ui::info(&format!("cached tokenId at {}", cache_file.display()));
    }

    ui::section("Phase A complete");
    ui::success(&format!(
        "deploy + manual relay finished ({})",
        ui::format_elapsed(phase_start)
    ));

    Ok((token_id, dest_token_addr))
}

// ---------------------------------------------------------------------------
// Helpers: encode payloads + relay second leg
// ---------------------------------------------------------------------------

/// Borsh-encode a HubMessage::SendToHub{ DeployInterchainToken } the way the
/// Solana ITS program does in `gmp::send_to_hub_wrap` + `encoding::HubMessage`.
/// Returns the raw bytes that get put on the wire to the cosmos hub.
fn encode_send_to_hub_deploy(
    destination_chain: &str,
    token_id: &[u8; 32],
    name: &str,
    symbol: &str,
    decimals: u8,
    minter: Option<Vec<u8>>,
) -> Result<Vec<u8>> {
    use solana_axelar_its::encoding::{DeployInterchainToken, HubMessage, Message};
    let inner = Message::DeployInterchainToken(DeployInterchainToken {
        token_id: *token_id,
        name: name.to_string(),
        symbol: symbol.to_string(),
        decimals,
        minter,
    });
    let hub = HubMessage::SendToHub {
        destination_chain: destination_chain.to_string(),
        message: inner,
    };
    borsh::to_vec(&hub).map_err(|e| eyre::eyre!("borsh encode failed: {e}"))
}

/// ABI-encode the inner ITS deploy payload destined for the EVM ITS proxy.
/// Format: `abi.encode(uint256 messageType=1, bytes32 tokenId, string name, string symbol, uint8 decimals, bytes minter)`.
/// Note: `uint8` and `uint256` produce identical 32-byte encodings in tuple position
/// for values that fit, so we widen to U256 for alloy's `abi_encode_params` tuple support.
fn encode_inner_deploy(
    token_id: &[u8; 32],
    name: &str,
    symbol: &str,
    decimals: u8,
    minter: &[u8],
) -> Vec<u8> {
    (
        crate::types::ItsMessageType::DeployInterchainToken.as_u256(),
        FixedBytes::<32>::from(*token_id),
        name.to_string(),
        symbol.to_string(),
        U256::from(decimals),
        Bytes::copy_from_slice(minter),
    )
        .abi_encode_params()
}

/// ABI-encode the inner ITS interchain-transfer payload.
/// Format: `abi.encode(uint256 messageType=0, bytes32 tokenId, bytes sourceAddress, bytes destinationAddress, uint256 amount, bytes data)`.
fn encode_inner_transfer(
    token_id: &[u8; 32],
    source_address: &[u8],
    destination_address: &[u8],
    amount: u64,
    data: &[u8],
) -> Vec<u8> {
    (
        crate::types::ItsMessageType::InterchainTransfer.as_u256(),
        FixedBytes::<32>::from(*token_id),
        Bytes::copy_from_slice(source_address),
        Bytes::copy_from_slice(destination_address),
        U256::from(amount),
        Bytes::copy_from_slice(data),
    )
        .abi_encode_params()
}

/// ABI-encode the outer hub envelope for an inbound ITS message.
/// Format: `abi.encode(uint256 messageType=4, string originalSourceChain, bytes innerPayload)`.
fn encode_receive_from_hub(
    original_source_chain: &crate::types::ChainAxelarId,
    inner: &[u8],
) -> Vec<u8> {
    (
        crate::types::ItsMessageType::ReceiveFromHub.as_u256(),
        original_source_chain.to_string(),
        Bytes::copy_from_slice(inner),
    )
        .abi_encode_params()
}

/// Drive the second leg actively: wait for hub-routed message → discover its
/// cc_id → wait for the destination cosm gateway to have it → construct_proof
/// on the destination MultisigProver → submit to the EVM gateway →
/// `ITS.execute(...)` on the destination ITS proxy. Returns the commandId.
#[allow(clippy::too_many_arguments)]
async fn relay_to_destination<P: Provider>(
    first_leg_message_id: &str,
    src_axelar_id: &crate::types::ChainAxelarId,
    dest_payload: &[u8],
    _dst_axelar_id: &crate::types::ChainAxelarId,
    _dst_chain_key: &str,
    dst_its_proxy: Address,
    dst_evm_gateway: Address,
    dst_provider: &P,
    signing_key: &cosmrs::crypto::secp256k1::SigningKey,
    axelar_address: &str,
    lcd: &str,
    chain_id: &str,
    fee_denom: &str,
    gas_price: f64,
    dst_cosm_gateway: &str,
    dst_multisig_prover: &str,
    axelarnet_gateway: &str,
    axelar_rpc: &str,
    step_base: usize,
    step_total: usize,
) -> Result<FixedBytes<32>> {
    // Wait until the AxelarnetGateway hub has approved the first-leg message.
    // executable_messages is keyed by the *source* chain of the message.
    ui::step_header(step_base, step_total, "Wait for hub approval");
    let spinner = ui::wait_spinner("Polling hub for approval...");
    let mut hub_approved = false;
    for i in 0..AMPLIFIER_POLL_ATTEMPTS_5MIN {
        if i > 0 {
            tokio::time::sleep(AMPLIFIER_POLL_INTERVAL).await;
        }
        if check_hub_approved(
            lcd,
            axelarnet_gateway,
            src_axelar_id.as_str(),
            first_leg_message_id,
        )
        .await
        .unwrap_or(false)
        {
            hub_approved = true;
            spinner.finish_and_clear();
            ui::success("hub approved first-leg message");
            break;
        }
        spinner.set_message(format!(
            "Waiting for hub approval (attempt {}/60)...",
            i + 1
        ));
    }
    if !hub_approved {
        spinner.finish_and_clear();
        ui::warn(
            "hub never reported the message as approved — proceeding anyway since it may have already been forwarded",
        );
    }

    // Discover the second-leg message_id
    ui::step_header(step_base + 1, step_total, "Discover second-leg cc_id");
    let spinner = ui::wait_spinner("Searching tendermint for hub-execute tx...");
    let second_leg = loop_discover_second_leg(axelar_rpc, first_leg_message_id, &spinner).await?;
    spinner.finish_and_clear();
    ui::kv("second-leg message_id", &second_leg.message_id);
    ui::kv("second-leg source_chain", &second_leg.source_chain);
    ui::kv(
        "second-leg destination_chain",
        &second_leg.destination_chain,
    );
    ui::kv("second-leg source_address", &second_leg.source_address);
    ui::kv(
        "second-leg destination_address",
        &second_leg.destination_address,
    );
    ui::kv("second-leg payload_hash", &second_leg.payload_hash);

    // Sanity-check our reconstruction
    let local_hash = keccak256(dest_payload);
    let expected_hash_str = second_leg
        .payload_hash
        .strip_prefix("0x")
        .unwrap_or(&second_leg.payload_hash)
        .to_lowercase();
    let local_hash_str = alloy::hex::encode(local_hash.as_slice());
    if local_hash_str != expected_hash_str {
        ui::warn("payload hash mismatch between local reconstruction and hub event:");
        ui::warn(&format!("  local:    0x{local_hash_str}"));
        ui::warn(&format!("  expected: 0x{expected_hash_str}"));
        return Err(eyre::eyre!(
            "payload hash mismatch — would cause ITS.execute to revert"
        ));
    }
    ui::success("payload hash matches second-leg event");

    // Wait until the destination cosmos Gateway has the outgoing message
    ui::step_header(
        step_base + 2,
        step_total,
        "Wait for destination cosmos gateway to publish",
    );
    let spinner = ui::wait_spinner("Polling destination cosm gateway...");
    let mut routed = false;
    for i in 0..AMPLIFIER_POLL_ATTEMPTS_10MIN {
        if i > 0 {
            tokio::time::sleep(AMPLIFIER_POLL_INTERVAL).await;
        }
        if check_cosmos_routed(
            lcd,
            dst_cosm_gateway,
            crate::types::HubChain::NAME,
            &second_leg.message_id,
        )
        .await
        .unwrap_or(false)
        {
            routed = true;
            spinner.finish_and_clear();
            ui::success("destination cosm gateway has the message");
            break;
        }
        spinner.set_message(format!("Waiting for routing (attempt {}/120)...", i + 1));
    }
    if !routed {
        spinner.finish_and_clear();
        return Err(eyre::eyre!(
            "destination cosm gateway never received second-leg message"
        ));
    }

    // construct_proof on destination MultisigProver
    ui::step_header(
        step_base + 3,
        step_total,
        "construct_proof on dest MultisigProver",
    );
    let construct_proof_msg = json!({
        "construct_proof": [{
            "source_chain": crate::types::HubChain::NAME,
            "message_id": second_leg.message_id,
        }]
    });
    let construct_any =
        build_execute_msg_any(axelar_address, dst_multisig_prover, &construct_proof_msg)?;
    let construct_resp = sign_and_broadcast_cosmos_tx(
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        vec![construct_any],
    )
    .await?;
    let session_id = extract_event_attr(&construct_resp, "multisig_session_id")?;
    ui::kv("multisig_session_id", &session_id);

    // Wait for proof
    ui::step_header(step_base + 4, step_total, "Wait for proof signing");
    let proof = wait_for_proof(lcd, dst_multisig_prover, &session_id).await?;
    ui::success("proof ready");

    let execute_data_hex = proof["status"]["completed"]["execute_data"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("no execute_data in proof response"))?;
    let execute_data = alloy::hex::decode(execute_data_hex)?;

    // Submit to EVM gateway
    ui::step_header(
        step_base + 5,
        step_total,
        "Submit proof to dest EVM gateway",
    );
    let approve_tx = TransactionRequest::default()
        .to(dst_evm_gateway)
        .input(Bytes::from(execute_data).into());
    let pending_approve = dst_provider.send_transaction(approve_tx).await?;
    let _approve_receipt = crate::evm::broadcast_and_log(pending_approve, "evm approve tx").await?;

    // Derive commandId locally from (sourceChain, messageId). The amplifier
    // gateway computes it as `keccak256(sourceChain || "_" || messageId)` and
    // this avoids racing the public relayer: when it submits the same proof
    // first, our approve tx is a no-op and emits no `ContractCallApproved`
    // event, so parsing the receipt logs would fail.
    let cmd_preimage = format!("axelar_{}", second_leg.message_id);
    let command_id = keccak256(cmd_preimage.as_bytes());
    ui::kv("commandId", &format!("{command_id}"));

    // Sanity: isContractCallApproved on the gateway with the values we'll pass to ITS.execute
    let gw = AxelarAmplifierGateway::new(dst_evm_gateway, dst_provider);
    let payload_hash_b32 = keccak256(dest_payload);
    let approved = gw
        .isContractCallApproved(
            command_id,
            crate::types::HubChain::NAME.to_string(),
            second_leg.source_address.clone(),
            dst_its_proxy,
            payload_hash_b32,
        )
        .call()
        .await?;
    ui::kv("isContractCallApproved", &format!("{approved}"));
    if !approved {
        return Err(eyre::eyre!(
            "gateway says message not approved for ITS proxy + hub source — check source_address case / encoding"
        ));
    }

    // Execute on destination ITS proxy
    ui::step_header(step_base + 6, step_total, "Execute on destination ITS");
    let its = InterchainTokenService::new(dst_its_proxy, dst_provider);
    let exec_call = its.execute(
        command_id,
        crate::types::HubChain::NAME.to_string(),
        second_leg.source_address.clone(),
        Bytes::copy_from_slice(dest_payload),
    );
    let pending_exec = exec_call.send().await?;
    let _exec_receipt = crate::evm::broadcast_and_log(pending_exec, "its execute tx").await?;

    Ok(command_id)
}

/// Poll `discover_second_leg` until it returns Some, with a spinner.
async fn loop_discover_second_leg(
    axelar_rpc: &str,
    first_leg_message_id: &str,
    spinner: &indicatif::ProgressBar,
) -> Result<SecondLegInfo> {
    for i in 0..AMPLIFIER_POLL_ATTEMPTS_5MIN {
        if i > 0 {
            tokio::time::sleep(AMPLIFIER_POLL_INTERVAL).await;
        }
        if let Some(info) = discover_second_leg(axelar_rpc, first_leg_message_id).await? {
            return Ok(info);
        }
        spinner.set_message(format!(
            "Searching for hub-execute tx (attempt {}/60)...",
            i + 1
        ));
    }
    Err(eyre::eyre!(
        "could not discover second-leg cc_id after 5 minutes"
    ))
}
