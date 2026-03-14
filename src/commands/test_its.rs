use std::path::PathBuf;
use std::time::Instant;

use alloy::{
    primitives::{Address, Bytes, FixedBytes, U256, keccak256},
    providers::ProviderBuilder,
    signers::local::PrivateKeySigner,
    sol_types::{SolEvent, SolValue},
};
use eyre::Result;
use serde_json::json;

use crate::cli::resolve_axelar_id;
use crate::commands::test_helpers::{extract_poll_id, wait_for_poll_votes};
use crate::preflight;
use crate::cosmos::{
    build_execute_msg_any, derive_axelar_wallet, lcd_cosmwasm_smart_query, read_axelar_config,
    read_axelar_contract_field, sign_and_broadcast_cosmos_tx,
};
use crate::evm::{
    ContractCall, ERC20, InterchainToken, InterchainTokenDeployed, InterchainTokenFactory,
    InterchainTokenService, Ownable,
};
use crate::state::read_state;
use crate::ui;
use crate::utils::read_contract_address;

const TOTAL_STEPS: usize = 10;

// Destination chain (Amplifier chain with an active relayer)
const DEST_CHAIN: &str = "flow";

// Token parameters
const TOKEN_NAME: &str = "Axe Test Token";
const TOKEN_SYMBOL: &str = "AXE";
const TOKEN_DECIMALS: u8 = 18;

pub async fn run(axelar_id: Option<String>) -> Result<()> {
    let axelar_id = resolve_axelar_id(axelar_id)?;
    let state = read_state(&axelar_id)?;
    let start = Instant::now();

    let rpc_url: String = state["rpcUrl"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("no rpcUrl in state"))?
        .to_string();
    let target_json = PathBuf::from(
        state["targetJson"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("no targetJson in state"))?,
    );

    let private_key = state["deployerPrivateKey"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("no deployerPrivateKey in state"))?
        .to_string();

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
    preflight::check_evm_balances(
        &rpc_url,
        &[("deployer", deployer_address)],
        &token_symbol,
    )
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
        ui::error(&format!(
            "\"{DEST_CHAIN}\" is not a trusted chain on the ITS at {its_proxy_addr}"
        ));

        // Query owner on source ITS to tell the user who can fix it
        let source_owner = Ownable::new(its_proxy_addr, &provider)
            .owner()
            .call()
            .await
            .ok();

        // Query owner on destination ITS
        let dest_provider = ProviderBuilder::new().connect_http(dest_rpc.parse()?);
        let flow_owner = Ownable::new(dest_its_addr, &dest_provider)
            .owner()
            .call()
            .await
            .ok();

        let mut lines: Vec<String> = vec![
            format!(
                "The ITS on {axelar_id} does not trust \"{DEST_CHAIN}\" as a destination chain."
            ),
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
        return Ok(());
    }
    ui::success(&format!("\"{DEST_CHAIN}\" is trusted on {axelar_id} ITS"));

    // ── Step 1: Deploy interchain token locally ─────────────────────────
    ui::step_header(1, TOTAL_STEPS, "Deploy interchain token");

    let salt = generate_salt();
    let initial_supply = U256::from(1000u64) * U256::from(10u64).pow(U256::from(TOKEN_DECIMALS));

    ui::kv("name", TOKEN_NAME);
    ui::kv("symbol", TOKEN_SYMBOL);
    ui::kv("decimals", &TOKEN_DECIMALS.to_string());
    ui::kv("initial supply", &format!("{initial_supply}"));
    ui::kv("salt", &format!("{salt}"));

    let factory = InterchainTokenFactory::new(its_factory_addr, &provider);
    let deploy_call = factory
        .deployInterchainToken(
            salt,
            TOKEN_NAME.to_string(),
            TOKEN_SYMBOL.to_string(),
            TOKEN_DECIMALS,
            initial_supply,
            deployer_address,
        )
        .value(U256::ZERO);

    let pending = deploy_call.send().await?;
    let tx_hash = *pending.tx_hash();
    ui::tx_hash("tx", &format!("{tx_hash}"));
    ui::info("waiting for confirmation...");

    let receipt = tokio::time::timeout(std::time::Duration::from_secs(120), pending.get_receipt())
        .await
        .map_err(|_| eyre::eyre!("tx {tx_hash} timed out after 120s"))??;

    ui::success(&format!(
        "confirmed in block {}",
        receipt.block_number.unwrap_or(0)
    ));

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
    ui::tx_hash("tx", &format!("{tx_hash}"));
    ui::info("waiting for confirmation...");

    let receipt = tokio::time::timeout(std::time::Duration::from_secs(120), pending.get_receipt())
        .await
        .map_err(|_| eyre::eyre!("tx {tx_hash} timed out after 120s"))??;

    ui::success(&format!(
        "confirmed in block {}",
        receipt.block_number.unwrap_or(0)
    ));

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

    let mnemonic = state["mnemonic"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("no mnemonic in state"))?;
    let (signing_key, axelar_address) = derive_axelar_wallet(mnemonic)?;
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
    let verify_msg = json!({ "verify_messages": [its_msg] });
    let verify_any = build_execute_msg_any(&axelar_address, &cosm_gateway, &verify_msg)?;
    let verify_resp = sign_and_broadcast_cosmos_tx(
        &signing_key,
        &axelar_address,
        &lcd,
        &chain_id,
        &fee_denom,
        gas_price,
        vec![verify_any],
    )
    .await?;

    if let Some(poll_id) = extract_poll_id(&verify_resp) {
        ui::kv("poll_id", &poll_id);

        // ── Step 4: Wait for poll votes + end poll ──────────────────────────
        ui::step_header(4, TOTAL_STEPS, "Wait for poll votes + end poll");
        wait_for_poll_votes(&lcd, &voting_verifier, &poll_id).await?;

        // End the poll — retry if it hasn't expired yet
        let spinner = ui::wait_spinner("Ending poll (waiting for block expiry)...");
        for attempt in 0..60 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
            let end_poll_msg = json!({ "end_poll": { "poll_id": poll_id } });
            let end_poll_any =
                build_execute_msg_any(&axelar_address, &voting_verifier, &end_poll_msg)?;
            match sign_and_broadcast_cosmos_tx(
                &signing_key,
                &axelar_address,
                &lcd,
                &chain_id,
                &fee_denom,
                gas_price,
                vec![end_poll_any],
            )
            .await
            {
                Ok(_) => {
                    spinner.finish_and_clear();
                    ui::success("poll ended");
                    break;
                }
                Err(e) => {
                    let msg = format!("{e}");
                    if msg.contains("cannot tally before poll end") {
                        spinner.set_message(format!(
                            "Poll not expired yet (attempt {})...",
                            attempt + 1
                        ));
                        continue;
                    }
                    spinner.finish_and_clear();
                    return Err(e);
                }
            }
        }
    } else {
        ui::info("no new poll created — message already being verified by active verifiers");
        ui::step_header(4, TOTAL_STEPS, "Wait for poll votes + end poll");
        ui::info("skipped (existing poll)");
    }

    // ── Step 5: route_messages ──────────────────────────────────────────
    ui::step_header(5, TOTAL_STEPS, "route_messages");
    let spinner = ui::wait_spinner("Routing message to hub...");
    for attempt in 0..60 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
        let route_msg = json!({ "route_messages": [its_msg] });
        let route_any = build_execute_msg_any(&axelar_address, &cosm_gateway, &route_msg)?;
        match sign_and_broadcast_cosmos_tx(
            &signing_key,
            &axelar_address,
            &lcd,
            &chain_id,
            &fee_denom,
            gas_price,
            vec![route_any],
        )
        .await
        {
            Ok(_) => {
                spinner.finish_and_clear();
                ui::success("message routed to hub");
                break;
            }
            Err(e) => {
                let msg = format!("{e}");
                if msg.contains("not verified") {
                    spinner.set_message(format!(
                        "Message not yet verified (attempt {}/60)...",
                        attempt + 1
                    ));
                    continue;
                }
                spinner.finish_and_clear();
                return Err(e);
            }
        }
    }

    // ── Step 6: Execute on AxelarnetGateway (hub) ───────────────────────
    ui::step_header(6, TOTAL_STEPS, "Execute on AxelarnetGateway (hub)");

    let axelarnet_gateway =
        read_axelar_contract_field(&target_json, "/axelar/contracts/AxelarnetGateway/address")?;
    ui::address("AxelarnetGateway", &axelarnet_gateway);

    // Wait for message to become executable (Router needs time to approve it)
    let exec_query = json!({
        "executable_messages": {
            "cc_ids": [{
                "source_chain": axelar_id,
                "message_id": message_id,
            }]
        }
    });
    let spinner = ui::wait_spinner("Waiting for message to be approved on hub...");
    for i in 0..120 {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
        let status = lcd_cosmwasm_smart_query(&lcd, &axelarnet_gateway, &exec_query).await?;
        let status_str = serde_json::to_string(&status)?;
        if !status_str.contains("null") && status_str.contains(&message_id) {
            spinner.finish_and_clear();
            ui::success("message approved on hub");
            break;
        }
        if i == 119 {
            spinner.finish_and_clear();
            return Err(eyre::eyre!(
                "message not approved on AxelarnetGateway after 10 minutes"
            ));
        }
        spinner.set_message(format!("Not yet approved (attempt {}/120)...", i + 1));
    }

    // Execute with payload
    let payload_hex = alloy::hex::encode(&payload);
    let execute_msg = json!({
        "execute": {
            "cc_id": {
                "message_id": message_id,
                "source_chain": axelar_id,
            },
            "payload": payload_hex,
        }
    });
    let execute_any = build_execute_msg_any(&axelar_address, &axelarnet_gateway, &execute_msg)?;
    match sign_and_broadcast_cosmos_tx(
        &signing_key,
        &axelar_address,
        &lcd,
        &chain_id,
        &fee_denom,
        gas_price,
        vec![execute_any],
    )
    .await
    {
        Ok(_) => {
            ui::success(&format!(
                "hub executed — message routed to {DEST_CHAIN} (relayer will handle delivery)"
            ));
        }
        Err(e) => {
            let msg = format!("{e}");
            if msg.contains("already executed") {
                ui::success(&format!(
                    "message already executed on hub by relayer — continuing to {DEST_CHAIN}"
                ));
            } else {
                return Err(e);
            }
        }
    }

    // ── Step 7: Poll destination chain to confirm token deployed ─────────
    ui::step_header(
        7,
        TOTAL_STEPS,
        &format!("Poll {DEST_CHAIN} for token deployment"),
    );

    let dest_provider = ProviderBuilder::new().connect_http(dest_rpc.parse()?);
    let dest_its = InterchainTokenService::new(dest_its_addr, &dest_provider);

    ui::address(&format!("{DEST_CHAIN} ITS"), &format!("{dest_its_addr}"));
    ui::kv("tokenId", &format!("{token_id}"));

    // Get the predicted token address on the destination chain
    let predicted_addr = dest_its
        .interchainTokenAddress(token_id)
        .call()
        .await
        .map_err(|e| eyre::eyre!("failed to query interchainTokenAddress on {DEST_CHAIN}: {e}"))?;
    ui::address("predicted token addr", &format!("{predicted_addr}"));

    let spinner = ui::wait_spinner(&format!("Waiting for token to appear on {DEST_CHAIN}..."));
    let mut deployed_addr = Address::ZERO;

    for i in 0..30 {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        }

        // Try calling name() on the predicted address — if it succeeds, the token is deployed
        // (get_code_at doesn't work reliably on some chains like Flow)
        let token = ERC20::new(predicted_addr, &dest_provider);
        match token.name().call().await {
            Ok(name) => {
                spinner.finish_and_clear();
                ui::success(&format!("Token responds to name() → \"{name}\""));
                deployed_addr = predicted_addr;
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

    if deployed_addr == Address::ZERO {
        ui::warn(&format!(
            "Token not yet deployed on {DEST_CHAIN} after 5 minutes"
        ));
        ui::info("The relayer may still be processing. Check axelarscan for status.");
        ui::kv("tokenId", &format!("{token_id}"));
    } else {
        ui::success(&format!("Token deployed on {DEST_CHAIN}!"));
        ui::address(
            &format!("token address ({DEST_CHAIN})"),
            &format!("{deployed_addr}"),
        );
    }

    // ── Step 8: Send interchain transfer ────────────────────────────────
    ui::step_header(8, TOTAL_STEPS, "Send interchain transfer");

    let transfer_amount = U256::from(100u64) * U256::from(10u64).pow(U256::from(TOKEN_DECIMALS));
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
    ui::tx_hash("tx", &format!("{tx_hash}"));
    ui::info("waiting for confirmation...");

    let receipt = tokio::time::timeout(std::time::Duration::from_secs(120), pending.get_receipt())
        .await
        .map_err(|_| eyre::eyre!("tx {tx_hash} timed out after 120s"))??;

    ui::success(&format!(
        "confirmed in block {}",
        receipt.block_number.unwrap_or(0)
    ));

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
    ui::step_header(10, TOTAL_STEPS, &format!("Verify transfer on {DEST_CHAIN}"));

    ui::address("token", &format!("{predicted_addr}"));
    ui::address("receiver", &format!("{receiver}"));

    let dest_token = ERC20::new(predicted_addr, &dest_provider);
    let spinner = ui::wait_spinner(&format!("Waiting for balance to appear on {DEST_CHAIN}..."));

    let mut final_balance = U256::ZERO;
    for i in 0..30 {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
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

    // verify_messages
    ui::info("verify_messages...");
    let verify_msg = json!({ "verify_messages": [msg] });
    let verify_any = build_execute_msg_any(axelar_address, cosm_gateway, &verify_msg)?;
    let verify_resp = sign_and_broadcast_cosmos_tx(
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        vec![verify_any],
    )
    .await?;

    if let Some(poll_id) = extract_poll_id(&verify_resp) {
        ui::kv("poll_id", &poll_id);
        wait_for_poll_votes(lcd, voting_verifier, &poll_id).await?;

        // End poll
        let spinner = ui::wait_spinner("Ending poll...");
        for attempt in 0..60 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
            let end_poll_msg = json!({ "end_poll": { "poll_id": poll_id } });
            let end_poll_any =
                build_execute_msg_any(axelar_address, voting_verifier, &end_poll_msg)?;
            match sign_and_broadcast_cosmos_tx(
                signing_key,
                axelar_address,
                lcd,
                chain_id,
                fee_denom,
                gas_price,
                vec![end_poll_any],
            )
            .await
            {
                Ok(_) => {
                    spinner.finish_and_clear();
                    ui::success("poll ended");
                    break;
                }
                Err(e) => {
                    let err = format!("{e}");
                    if err.contains("cannot tally before poll end") {
                        spinner.set_message(format!(
                            "Poll not expired yet (attempt {})...",
                            attempt + 1
                        ));
                        continue;
                    }
                    spinner.finish_and_clear();
                    return Err(e);
                }
            }
        }
    } else {
        ui::info("no new poll — already being verified by active verifiers");
    }

    // route_messages
    ui::info("route_messages...");
    let spinner = ui::wait_spinner("Routing message to hub...");
    for attempt in 0..60 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
        let route_msg = json!({ "route_messages": [msg] });
        let route_any = build_execute_msg_any(axelar_address, cosm_gateway, &route_msg)?;
        match sign_and_broadcast_cosmos_tx(
            signing_key,
            axelar_address,
            lcd,
            chain_id,
            fee_denom,
            gas_price,
            vec![route_any],
        )
        .await
        {
            Ok(_) => {
                spinner.finish_and_clear();
                ui::success("message routed");
                break;
            }
            Err(e) => {
                let err = format!("{e}");
                if err.contains("not verified") {
                    spinner
                        .set_message(format!("Not yet verified (attempt {}/60)...", attempt + 1));
                    continue;
                }
                spinner.finish_and_clear();
                return Err(e);
            }
        }
    }

    // Execute on AxelarnetGateway
    ui::info("execute on AxelarnetGateway...");
    let exec_query = json!({
        "executable_messages": {
            "cc_ids": [{ "source_chain": axelar_id, "message_id": message_id }]
        }
    });
    let spinner = ui::wait_spinner("Waiting for approval on hub...");
    for i in 0..120 {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
        let status = lcd_cosmwasm_smart_query(lcd, axelarnet_gateway, &exec_query).await?;
        let status_str = serde_json::to_string(&status)?;
        if !status_str.contains("null") && status_str.contains(message_id) {
            spinner.finish_and_clear();
            ui::success("message approved on hub");
            break;
        }
        if i == 119 {
            spinner.finish_and_clear();
            return Err(eyre::eyre!(
                "message not approved on AxelarnetGateway after 10 minutes"
            ));
        }
        spinner.set_message(format!("Not yet approved (attempt {}/120)...", i + 1));
    }

    let payload_hex = alloy::hex::encode(payload);
    let execute_msg = json!({
        "execute": {
            "cc_id": { "message_id": message_id, "source_chain": axelar_id },
            "payload": payload_hex,
        }
    });
    let execute_any = build_execute_msg_any(axelar_address, axelarnet_gateway, &execute_msg)?;
    match sign_and_broadcast_cosmos_tx(
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        vec![execute_any],
    )
    .await
    {
        Ok(_) => ui::success("hub executed — message forwarded to destination"),
        Err(e) => {
            let err = format!("{e}");
            if err.contains("already executed") {
                ui::success("already executed by relayer");
            } else {
                return Err(e);
            }
        }
    }

    Ok(())
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
