use std::fs;
use std::path::Path;

use alloy::{
    hex,
    primitives::{Address, FixedBytes, U256},
};
use base64::Engine;
use bip32::Mnemonic;
use cosmos_sdk_proto::cosmos::base::v1beta1::Coin as ProtoCoin;
use cosmos_sdk_proto::cosmos::gov::v1::MsgSubmitProposal;
use cosmos_sdk_proto::cosmwasm::wasm::v1::MsgExecuteContract as ProtoMsgExecuteContract;
use cosmrs::bip32::XPrv;
use cosmrs::crypto::secp256k1::SigningKey;
use cosmrs::tx::{self, Fee, SignDoc, SignerInfo};
use eyre::Result;
use prost::Message;
use serde_json::Value;

use crate::evm::pubkey_to_address;
use crate::ui;

// --- wallet ---

pub fn derive_axelar_wallet(mnemonic_str: &str) -> Result<(SigningKey, String)> {
    let mnemonic = Mnemonic::new(mnemonic_str, bip32::Language::English)
        .map_err(|e| eyre::eyre!("invalid mnemonic: {e}"))?;
    let seed = mnemonic.to_seed("");
    let path: cosmrs::bip32::DerivationPath = "m/44'/118'/0'/0/0"
        .parse()
        .map_err(|e| eyre::eyre!("invalid derivation path: {e}"))?;
    let child_xprv = XPrv::derive_from_path(seed, &path)
        .map_err(|e| eyre::eyre!("key derivation failed: {e}"))?;
    let signing_key = SigningKey::from_slice(&child_xprv.private_key().to_bytes())
        .map_err(|e| eyre::eyre!("invalid signing key: {e}"))?;
    let account_id = signing_key
        .public_key()
        .account_id("axelar")
        .map_err(|e| eyre::eyre!("account id derivation failed: {e}"))?;
    Ok((signing_key, account_id.to_string()))
}

// --- LCD REST queries ---

pub async fn lcd_query_account(lcd: &str, address: &str) -> Result<(u64, u64)> {
    let url = format!("{lcd}/cosmos/auth/v1beta1/accounts/{address}");
    let resp: Value = reqwest::get(&url).await?.json().await?;
    let account = resp
        .get("account")
        .ok_or_else(|| eyre::eyre!("no account in response: {resp}"))?;
    let account_number: u64 = account["account_number"]
        .as_str()
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    let sequence: u64 = account["sequence"]
        .as_str()
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    Ok((account_number, sequence))
}

/// Query the bank balance of `address` for a single denom. Returns the amount in base units (e.g. uaxl).
pub async fn lcd_query_balance(lcd: &str, address: &str, denom: &str) -> Result<u128> {
    let url = format!("{lcd}/cosmos/bank/v1beta1/balances/{address}/by_denom?denom={denom}");
    let resp: Value = reqwest::get(&url).await?.json().await?;
    let amount: u128 = resp
        .pointer("/balance/amount")
        .and_then(|v| v.as_str())
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    Ok(amount)
}

/// Preflight: ensure the Axelar relayer wallet exists on-chain and holds enough fee-denom to relay.
/// Errors with a clear "fund this address" message if the account is missing or balance is below `min_amount`.
pub async fn check_axelar_balance(
    lcd: &str,
    chain_id: &str,
    address: &str,
    fee_denom: &str,
    min_amount: u128,
) -> Result<()> {
    let account_exists = lcd_query_account(lcd, address).await.is_ok();
    let balance = lcd_query_balance(lcd, address, fee_denom)
        .await
        .unwrap_or(0);

    let display = balance as f64 / 1_000_000.0;
    let min_display = min_amount as f64 / 1_000_000.0;

    if !account_exists || balance < min_amount {
        ui::error(&format!("axelar relayer wallet underfunded on {chain_id}:"));
        ui::error(&format!("  address: {address}"));
        ui::error(&format!(
            "  balance: {display:.6} {fee_denom} (need >= {min_display:.6})"
        ));
        if !account_exists {
            ui::error("  account does not exist on-chain — send any amount to create it");
        }
        return Err(eyre::eyre!(
            "fund {address} with at least {min_display:.6} {fee_denom} on {chain_id} and retry"
        ));
    }

    ui::kv(
        "relayer balance",
        &format!("{display:.6} {fee_denom} (>= {min_display:.6})"),
    );
    Ok(())
}

pub async fn lcd_simulate_tx(lcd: &str, tx_bytes: &[u8]) -> Result<u64> {
    let tx_b64 = base64::engine::general_purpose::STANDARD.encode(tx_bytes);
    let body = serde_json::json!({
        "tx_bytes": tx_b64,
        "mode": "BROADCAST_MODE_UNSPECIFIED"
    });
    let client = reqwest::Client::new();
    let resp: Value = client
        .post(format!("{lcd}/cosmos/tx/v1beta1/simulate"))
        .json(&body)
        .send()
        .await?
        .json()
        .await?;
    if let Some(err) = resp.get("message").and_then(|v| v.as_str())
        && !err.is_empty()
    {
        return Err(eyre::eyre!("simulation failed: {err}"));
    }
    let gas_used: u64 = resp
        .pointer("/gas_info/gas_used")
        .and_then(|v| v.as_str())
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    if gas_used == 0 {
        return Err(eyre::eyre!(
            "simulation returned 0 gas — response: {}",
            serde_json::to_string_pretty(&resp)?
        ));
    }
    Ok(gas_used)
}

pub async fn lcd_broadcast_tx(lcd: &str, tx_bytes: &[u8]) -> Result<Value> {
    let tx_b64 = base64::engine::general_purpose::STANDARD.encode(tx_bytes);
    let body = serde_json::json!({
        "tx_bytes": tx_b64,
        "mode": "BROADCAST_MODE_SYNC"
    });
    let client = reqwest::Client::new();
    let resp: Value = client
        .post(format!("{lcd}/cosmos/tx/v1beta1/txs"))
        .json(&body)
        .send()
        .await?
        .json()
        .await?;
    let code = resp
        .pointer("/tx_response/code")
        .and_then(|v| v.as_u64())
        .unwrap_or(1);
    if code != 0 {
        let raw_log = resp
            .pointer("/tx_response/raw_log")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Err(eyre::eyre!("broadcast failed (code {code}): {raw_log}"));
    }
    Ok(resp)
}

/// Wait for a tx to be included in a block and return the full tx response with events.
pub async fn lcd_wait_for_tx(lcd: &str, tx_hash: &str) -> Result<Value> {
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let url = format!("{lcd}/cosmos/tx/v1beta1/txs/{tx_hash}");
        let resp: Value = match reqwest::get(&url).await {
            Ok(r) => r.json().await.unwrap_or(serde_json::json!({})),
            Err(_) => continue,
        };
        if resp.get("tx_response").is_some() {
            let code = resp
                .pointer("/tx_response/code")
                .and_then(|v| v.as_u64())
                .unwrap_or(1);
            if code != 0 {
                let raw_log = resp
                    .pointer("/tx_response/raw_log")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                return Err(eyre::eyre!("tx failed (code {code}): {raw_log}"));
            }
            return Ok(resp);
        }
    }
    Err(eyre::eyre!("timeout waiting for tx {tx_hash}"))
}

pub async fn lcd_query_proposal(lcd: &str, proposal_id: u64) -> Result<Value> {
    let url = format!("{lcd}/cosmos/gov/v1/proposals/{proposal_id}");
    let resp: Value = reqwest::get(&url).await?.json().await?;
    let proposal = resp
        .get("proposal")
        .cloned()
        .ok_or_else(|| eyre::eyre!("no 'proposal' field in response"))?;
    Ok(proposal)
}

pub async fn lcd_cosmwasm_smart_query(
    lcd: &str,
    contract: &str,
    query_msg: &Value,
) -> Result<Value> {
    let query_json = serde_json::to_string(query_msg)?;
    let query_b64 = base64::engine::general_purpose::STANDARD.encode(query_json.as_bytes());
    let url = format!("{lcd}/cosmwasm/wasm/v1/contract/{contract}/smart/{query_b64}");
    let resp: Value = reqwest::get(&url).await?.json().await?;
    Ok(resp["data"].clone())
}

/// Fetch code IDs by matching storeCodeProposalCodeHash against on-chain checksums.
pub async fn lcd_fetch_code_id(lcd: &str, expected_checksum: &str) -> Result<u64> {
    let expected = expected_checksum.to_uppercase();
    let mut next_key: Option<String> = None;
    loop {
        let mut url =
            format!("{lcd}/cosmwasm/wasm/v1/code?pagination.limit=100&pagination.reverse=true");
        if let Some(ref key) = next_key {
            url.push_str(&format!("&pagination.key={key}"));
        }
        let resp: Value = reqwest::get(&url).await?.json().await?;
        let codes = resp["code_infos"]
            .as_array()
            .ok_or_else(|| eyre::eyre!("no code_infos in response"))?;
        for code in codes {
            let checksum = code["data_hash"].as_str().unwrap_or("").to_uppercase();
            if checksum == expected {
                let code_id: u64 = code["code_id"].as_str().unwrap_or("0").parse().unwrap_or(0);
                return Ok(code_id);
            }
        }
        let nk = resp
            .pointer("/pagination/next_key")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if nk.is_empty() {
            break;
        }
        next_key = Some(nk.to_string());
    }
    Err(eyre::eyre!(
        "code not found for checksum {expected_checksum}"
    ))
}

// --- tx building ---

#[allow(clippy::too_many_arguments)]
pub fn build_and_sign_cosmos_tx(
    signing_key: &SigningKey,
    chain_id: &str,
    account_number: u64,
    sequence: u64,
    gas_limit: u64,
    fee_amount: u128,
    fee_denom: &str,
    messages: Vec<cosmrs::Any>,
) -> Result<Vec<u8>> {
    let tx_body = tx::Body::new(messages, "", 0u32);
    let signer_info = SignerInfo::single_direct(Some(signing_key.public_key()), sequence);
    let fee = Fee::from_amount_and_gas(
        cosmrs::Coin {
            denom: fee_denom
                .parse()
                .map_err(|e| eyre::eyre!("invalid denom: {e}"))?,
            amount: fee_amount,
        },
        gas_limit,
    );
    let auth_info = signer_info.auth_info(fee);
    let cosmos_chain_id: cosmrs::tendermint::chain::Id = chain_id
        .parse()
        .map_err(|e| eyre::eyre!("invalid chain id: {e}"))?;
    let sign_doc = SignDoc::new(&tx_body, &auth_info, &cosmos_chain_id, account_number)
        .map_err(|e| eyre::eyre!("sign doc error: {e}"))?;
    let tx_signed = sign_doc
        .sign(signing_key)
        .map_err(|e| eyre::eyre!("signing error: {e}"))?;
    let tx_bytes = tx_signed
        .to_bytes()
        .map_err(|e| eyre::eyre!("serialize error: {e}"))?;
    Ok(tx_bytes)
}

/// Build a MsgExecuteContract as protobuf Any
pub fn build_execute_msg_any(
    sender: &str,
    contract: &str,
    msg_json: &Value,
) -> Result<cosmrs::Any> {
    build_execute_msg_any_with_funds(sender, contract, msg_json, vec![])
}

pub fn build_execute_msg_any_with_funds(
    sender: &str,
    contract: &str,
    msg_json: &Value,
    funds: Vec<ProtoCoin>,
) -> Result<cosmrs::Any> {
    let msg_bytes = serde_json::to_vec(msg_json)?;
    let proto_msg = ProtoMsgExecuteContract {
        sender: sender.to_string(),
        contract: contract.to_string(),
        msg: msg_bytes,
        funds,
    };
    let mut buf = Vec::new();
    proto_msg.encode(&mut buf)?;
    Ok(cosmrs::Any {
        type_url: "/cosmwasm.wasm.v1.MsgExecuteContract".to_string(),
        value: buf,
    })
}

/// Wrap execute messages in a MsgSubmitProposal as protobuf Any
pub fn build_submit_proposal_any(
    proposer: &str,
    inner_messages: Vec<cosmrs::Any>,
    title: &str,
    summary: &str,
    deposit_amount: &str,
    deposit_denom: &str,
    expedited: bool,
) -> Result<cosmrs::Any> {
    let prost_messages: Vec<tendermint_proto::google::protobuf::Any> = inner_messages
        .into_iter()
        .map(|a| tendermint_proto::google::protobuf::Any {
            type_url: a.type_url,
            value: a.value,
        })
        .collect();

    let proposal = MsgSubmitProposal {
        messages: prost_messages,
        initial_deposit: vec![ProtoCoin {
            denom: deposit_denom.to_string(),
            amount: deposit_amount.to_string(),
        }],
        proposer: proposer.to_string(),
        metadata: String::new(),
        title: title.to_string(),
        summary: summary.to_string(),
        expedited,
    };
    let mut buf = Vec::new();
    proposal.encode(&mut buf)?;
    Ok(cosmrs::Any {
        type_url: "/cosmos.gov.v1.MsgSubmitProposal".to_string(),
        value: buf,
    })
}

/// Sign, simulate, re-sign with correct gas, broadcast, and wait for inclusion.
pub async fn sign_and_broadcast_cosmos_tx(
    signing_key: &SigningKey,
    address: &str,
    lcd: &str,
    chain_id: &str,
    fee_denom: &str,
    gas_price: f64,
    messages: Vec<cosmrs::Any>,
) -> Result<Value> {
    let (account_number, sequence) = lcd_query_account(lcd, address).await?;
    ui::kv(
        "account",
        &format!("{address}, number={account_number}, sequence={sequence}"),
    );

    let sim_tx = build_and_sign_cosmos_tx(
        signing_key,
        chain_id,
        account_number,
        sequence,
        10_000_000,
        0,
        fee_denom,
        messages.clone(),
    )?;

    let gas_used = lcd_simulate_tx(lcd, &sim_tx).await?;
    let gas_limit = (gas_used as f64 * 2.0) as u64;
    let fee_amount = ((gas_limit as f64) * gas_price).ceil() as u128;
    ui::kv(
        "gas",
        &format!("used={gas_used}, limit={gas_limit}, fee={fee_amount}{fee_denom}"),
    );

    let tx_bytes = build_and_sign_cosmos_tx(
        signing_key,
        chain_id,
        account_number,
        sequence,
        gas_limit,
        fee_amount,
        fee_denom,
        messages,
    )?;

    let broadcast_resp = lcd_broadcast_tx(lcd, &tx_bytes).await?;
    let tx_hash = broadcast_resp
        .pointer("/tx_response/txhash")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    ui::tx_hash("tx hash", &tx_hash);

    ui::info("waiting for tx confirmation...");
    let tx_resp = lcd_wait_for_tx(lcd, &tx_hash).await?;
    Ok(tx_resp)
}

/// Extract proposal_id from tx response events
pub fn extract_proposal_id(tx_resp: &Value) -> Result<u64> {
    let events = tx_resp
        .pointer("/tx_response/events")
        .and_then(|v| v.as_array())
        .ok_or_else(|| eyre::eyre!("no events in tx response"))?;
    for event in events {
        let event_type = event["type"].as_str().unwrap_or("");
        if (event_type == "submit_proposal" || event_type == "proposal_submitted")
            && let Some(attrs) = event["attributes"].as_array()
        {
            for attr in attrs {
                let key = attr["key"].as_str().unwrap_or("");
                if key == "proposal_id" {
                    let val = attr["value"].as_str().unwrap_or("0");
                    return Ok(val.parse()?);
                }
            }
        }
    }
    Err(eyre::eyre!("proposal_id not found in tx events"))
}

/// Read Axelar LCD url, chain ID, fee denom, and gas price from target json
pub fn read_axelar_config(target_json: &Path) -> Result<(String, String, String, f64)> {
    let content = fs::read_to_string(target_json)?;
    let root: Value = serde_json::from_str(&content)?;
    let lcd = root
        .pointer("/axelar/lcd")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("no axelar.lcd in target json"))?
        .to_string();
    let chain_id = root
        .pointer("/axelar/chainId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("no axelar.chainId in target json"))?
        .to_string();
    let gas_price_str = root
        .pointer("/axelar/gasPrice")
        .and_then(|v| v.as_str())
        .unwrap_or("0.007uaxl");
    let (price_num, denom) = parse_gas_price(gas_price_str);
    Ok((lcd, chain_id, denom, price_num))
}

fn parse_gas_price(s: &str) -> (f64, String) {
    let mut split_at = 0;
    for (i, c) in s.char_indices() {
        if c.is_alphabetic() {
            split_at = i;
            break;
        }
    }
    if split_at == 0 {
        return (0.007, "uaxl".to_string());
    }
    let price: f64 = s[..split_at].parse().unwrap_or(0.007);
    let denom = s[split_at..].to_string();
    (price, denom)
}

/// Read a string field from axelar contracts config
pub fn read_axelar_contract_field(target_json: &Path, pointer: &str) -> Result<String> {
    let content = fs::read_to_string(target_json)?;
    let root: Value = serde_json::from_str(&content)?;
    root.pointer(pointer)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| eyre::eyre!("field not found: {pointer}"))
}

/// Read Axelar RPC url from target json (`/axelar/rpc`).
pub fn read_axelar_rpc(target_json: &Path) -> Result<String> {
    let content = fs::read_to_string(target_json)?;
    let root: Value = serde_json::from_str(&content)?;
    root.pointer("/axelar/rpc")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| eyre::eyre!("no axelar.rpc in target json"))
}

/// Query Tendermint RPC `tx_search` for a single event key/value pair.
/// Returns the parsed JSON response.
pub async fn rpc_tx_search_event(rpc: &str, event_key: &str, event_value: &str) -> Result<Value> {
    let url = format!("{rpc}/tx_search?query=\"{event_key}='{event_value}'\"&per_page=1");
    let resp = reqwest::get(&url).await?.json::<Value>().await?;
    Ok(resp)
}

/// Query Tendermint RPC `tx_search` with a raw query string (e.g.
/// `key1='value' AND key2='value'`). Returns the parsed `result` payload (with
/// keys `total_count`, `txs`).
pub async fn rpc_tx_search(
    rpc: &str,
    query: &str,
    per_page: u32,
    page: u32,
    order_desc: bool,
) -> Result<Value> {
    let json_quoted = serde_json::to_string(query)?;
    let order = if order_desc { "\"desc\"" } else { "\"asc\"" };
    let resp = reqwest::Client::new()
        .get(format!("{rpc}/tx_search"))
        .query(&[
            ("query", json_quoted.as_str()),
            ("per_page", &per_page.to_string()),
            ("page", &page.to_string()),
            ("order_by", order),
        ])
        .send()
        .await?
        .json::<Value>()
        .await?;
    Ok(resp.get("result").cloned().unwrap_or(Value::Null))
}

/// Query Tendermint RPC `block` endpoint for a given height. Returns block.header.time.
pub async fn rpc_block_time(rpc: &str, height: u64) -> Result<String> {
    let url = format!("{rpc}/block?height={height}");
    let resp: Value = reqwest::get(&url).await?.json().await?;
    Ok(resp
        .pointer("/result/block/header/time")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default())
}

/// Fetch the current verifier set from Axelar chain via LCD REST endpoint.
/// Returns (signers sorted by address, threshold, nonce, verifierSetId)
pub async fn fetch_verifier_set(
    target_json: &Path,
    chain_axelar_id: &str,
) -> Result<(Vec<(Address, u128)>, u128, FixedBytes<32>, String)> {
    let content = fs::read_to_string(target_json)?;
    let root: Value = serde_json::from_str(&content)?;

    let lcd = root
        .pointer("/axelar/lcd")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("no axelar.lcd in target json"))?;

    let prover_addr = root
        .pointer(&format!(
            "/axelar/contracts/MultisigProver/{chain_axelar_id}/address"
        ))
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("no MultisigProver.{chain_axelar_id}.address in target json"))?;

    let query_msg = "\"current_verifier_set\"";
    let query_b64 = base64::engine::general_purpose::STANDARD.encode(query_msg.as_bytes());

    let url = format!("{lcd}/cosmwasm/wasm/v1/contract/{prover_addr}/smart/{query_b64}");
    ui::info(&format!("fetching verifier set from: {url}"));

    let resp: Value = reqwest::get(&url).await?.json().await?;

    let data = &resp["data"];
    let verifier_set_id = data["id"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("no id in verifier set response"))?
        .to_string();

    let verifier_set = &data["verifier_set"];
    let signers_obj = verifier_set["signers"]
        .as_object()
        .ok_or_else(|| eyre::eyre!("no signers object in verifier set"))?;

    let threshold: u128 = verifier_set["threshold"]
        .as_str()
        .or_else(|| verifier_set["threshold"].as_u64().map(|_| ""))
        .ok_or_else(|| eyre::eyre!("no threshold in verifier set"))
        .and_then(|s| {
            if s.is_empty() {
                Ok(verifier_set["threshold"].as_u64().unwrap() as u128)
            } else {
                s.parse::<u128>()
                    .map_err(|e| eyre::eyre!("invalid threshold: {e}"))
            }
        })?;

    let created_at = verifier_set["created_at"]
        .as_u64()
        .ok_or_else(|| eyre::eyre!("no created_at in verifier set"))?;

    let nonce = FixedBytes::<32>::from(U256::from(created_at).to_be_bytes::<32>());

    let mut weighted_signers: Vec<(Address, u128)> = Vec::new();

    for (_key, signer) in signers_obj {
        let pubkey_hex = signer
            .pointer("/pub_key/ecdsa")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre::eyre!("no pub_key.ecdsa for signer"))?;

        let weight: u128 = signer["weight"]
            .as_str()
            .map(|s| s.parse::<u128>())
            .unwrap_or_else(|| Ok(signer["weight"].as_u64().unwrap_or(1) as u128))
            .map_err(|e| eyre::eyre!("invalid weight: {e}"))?;

        let pubkey_bytes = hex::decode(pubkey_hex.strip_prefix("0x").unwrap_or(pubkey_hex))?;
        let addr = pubkey_to_address(&pubkey_bytes)?;
        weighted_signers.push((addr, weight));
    }

    weighted_signers.sort_by_key(|(addr, _)| *addr);

    ui::kv(
        "verifier set",
        &format!(
            "{} signers, threshold={}, created_at={}, id={}",
            weighted_signers.len(),
            threshold,
            created_at,
            verifier_set_id
        ),
    );
    for (addr, weight) in &weighted_signers {
        ui::kv(&format!("{addr}"), &format!("weight={weight}"));
    }

    Ok((weighted_signers, threshold, nonce, verifier_set_id))
}
