use eyre::{Result, bail};
use owo_colors::OwoColorize;
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_transaction_status::option_serializer::OptionSerializer;
use solana_transaction_status::{
    EncodedConfirmedTransactionWithStatusMeta, UiInstruction, UiTransactionEncoding,
};
use std::collections::HashMap;
use std::str::FromStr;

// ---------------------------------------------------------------------------
// Known program IDs (resolved at runtime from crates)
// ---------------------------------------------------------------------------

fn known_programs() -> HashMap<Pubkey, &'static str> {
    let mut m = HashMap::new();
    m.insert(solana_axelar_gateway::id(), "AxelarGateway");
    m.insert(solana_axelar_gas_service::id(), "AxelarGasService");
    m.insert(solana_axelar_memo::id(), "AxelarMemo");
    m.insert(solana_axelar_its::id(), "AxelarITS");
    m
}

// ---------------------------------------------------------------------------
// Anchor instruction discriminators (sha256("global:<name>")[0:8])
// ---------------------------------------------------------------------------

fn instruction_name(discriminator: &[u8]) -> Option<&'static str> {
    if discriminator.len() < 8 {
        return None;
    }
    match discriminator[..8] {
        // Gateway
        [0xb1, 0x96, 0x55, 0x82, 0x81, 0x5c, 0xbc, 0xd3] => Some("CallContract"),
        [0xd0, 0x7f, 0x15, 0x01, 0xc2, 0xbe, 0xc4, 0x46] => Some("InitializeConfig"),
        [0x88, 0xc9, 0xf1, 0x4a, 0x08, 0xed, 0x3f, 0xe7] => {
            Some("InitializePayloadVerificationSession")
        }
        [0x5b, 0x8b, 0x18, 0x45, 0xfb, 0xa2, 0xf5, 0x70] => Some("VerifySignature"),
        [0x41, 0x9a, 0x84, 0x87, 0x69, 0x05, 0xad, 0x15] => Some("ApproveMessage"),
        [0xed, 0xe5, 0xc8, 0xc1, 0x07, 0xe5, 0xd4, 0x7f] => Some("ValidateMessage"),
        [0x7a, 0xc4, 0xe7, 0x9f, 0xa3, 0x18, 0xcf, 0xa6] => Some("RotateSigners"),
        [0x11, 0xee, 0x56, 0xd0, 0xe9, 0x7a, 0xc3, 0xba] => Some("TransferOperatorship"),
        // GasService
        [0x4e, 0x8c, 0xae, 0x08, 0xbc, 0xe8, 0xef, 0x03] => Some("PayGas"),
        // Memo
        [0xce, 0xb2, 0x4f, 0x13, 0x3f, 0xd2, 0x48, 0xef] => Some("SendMemo"),
        // ITS
        [0xd0, 0xc6, 0xf5, 0x3c, 0x87, 0xe4, 0x64, 0xdc] => Some("InterchainTransfer"),
        [0xcc, 0x91, 0x2f, 0x06, 0x85, 0x07, 0x51, 0xc8] => Some("DeployInterchainToken"),
        [0xdf, 0x5b, 0x11, 0x99, 0x5f, 0xec, 0x7e, 0xc6] => Some("DeployRemoteInterchainToken"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Anchor event discriminators (sha256("event:<Name>")[0:8])
// Anchor CPI events are prefixed with EVENT_IX_TAG_LE (8 bytes) then the
// event discriminator (8 bytes), then borsh-encoded event data.
// ---------------------------------------------------------------------------

const EVENT_IX_TAG_LE: &[u8] = &[0xe4, 0x45, 0xa5, 0x2e, 0x51, 0xcb, 0x9a, 0x1d];

fn event_name(discriminator: &[u8]) -> Option<&'static str> {
    if discriminator.len() < 8 {
        return None;
    }
    match discriminator[..8] {
        [0xd3, 0xd3, 0x50, 0x7e, 0x96, 0x62, 0xb5, 0xc6] => Some("CallContractEvent"),
        [0xfa, 0xfe, 0x1d, 0xe3, 0x9f, 0xcd, 0x72, 0x59] => Some("MessageApprovedEvent"),
        [0x09, 0x9d, 0xbc, 0xe1, 0xa8, 0x1a, 0x5e, 0x52] => Some("MessageExecutedEvent"),
        [0x36, 0x4f, 0x98, 0x9b, 0x8a, 0x44, 0xe5, 0x60] => Some("VerifierSetRotatedEvent"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Decode CallContractEvent from borsh
// ---------------------------------------------------------------------------

fn try_decode_call_contract_event(data: &[u8]) -> Option<String> {
    let event = borsh::from_slice::<solana_axelar_gateway::events::CallContractEvent>(data).ok()?;
    Some(format!(
        "sender: {}\n      destination_chain: \"{}\"\n      destination_address: \"{}\"\n      payload_hash: {}\n      payload: {} bytes",
        event.sender,
        event.destination_chain,
        event.destination_contract_address,
        hex::encode(event.payload_hash),
        event.payload.len(),
    ))
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

pub async fn run(txid: &str, solana_rpc: &str) -> Result<()> {
    let sig =
        Signature::from_str(txid).map_err(|e| eyre::eyre!("invalid Solana signature: {e}"))?;

    let rpc = RpcClient::new_with_commitment(solana_rpc, CommitmentConfig::confirmed());

    let tx_data: EncodedConfirmedTransactionWithStatusMeta = rpc
        .get_transaction_with_config(
            &sig,
            solana_client::rpc_config::RpcTransactionConfig {
                encoding: Some(UiTransactionEncoding::Json),
                commitment: Some(CommitmentConfig::confirmed()),
                max_supported_transaction_version: Some(0),
            },
        )
        .map_err(|e| eyre::eyre!("failed to fetch transaction: {e}"))?;

    let slot = tx_data.slot;
    let block_time = tx_data.block_time.unwrap_or(0);

    let meta = tx_data
        .transaction
        .meta
        .as_ref()
        .ok_or_else(|| eyre::eyre!("transaction has no metadata"))?;

    let status = if meta.err.is_some() {
        "Failed"
    } else {
        "Success"
    };

    // Extract account keys
    let account_keys: Vec<Pubkey> = match &tx_data.transaction.transaction {
        solana_transaction_status::EncodedTransaction::Json(ui_tx) => match &ui_tx.message {
            solana_transaction_status::UiMessage::Raw(raw) => raw
                .account_keys
                .iter()
                .filter_map(|k| Pubkey::from_str(k).ok())
                .collect(),
            _ => vec![],
        },
        _ => vec![],
    };

    // Get loaded addresses (from ALTs)
    let mut all_keys = account_keys.clone();
    if let OptionSerializer::Some(loaded) = &meta.loaded_addresses {
        for k in &loaded.writable {
            if let Ok(pk) = Pubkey::from_str(k) {
                all_keys.push(pk);
            }
        }
        for k in &loaded.readonly {
            if let Ok(pk) = Pubkey::from_str(k) {
                all_keys.push(pk);
            }
        }
    }

    let known = known_programs();

    let logs: Vec<String> = match &meta.log_messages {
        OptionSerializer::Some(logs) => logs.clone(),
        _ => vec![],
    };

    let compute_units = match &meta.compute_units_consumed {
        OptionSerializer::Some(cu) => Some(*cu),
        _ => None,
    };

    // Print header
    println!("{} {}", "Tx:".bold(), txid);
    println!("{} {}", "Slot:".bold(), slot);
    if block_time > 0 {
        println!("{} {}", "Time:".bold(), block_time);
    }
    println!(
        "{} {}",
        "Status:".bold(),
        if status == "Success" {
            status.green().to_string()
        } else {
            status.red().to_string()
        }
    );
    if let Some(cu) = compute_units {
        println!("{} {}", "Compute Units:".bold(), cu);
    }
    {
        #[allow(clippy::float_arithmetic)]
        let fee = meta.fee as f64 / 1e9;
        println!("{} ◎{fee}", "Fee:".bold());
    }

    // Print top-level instructions
    if let solana_transaction_status::EncodedTransaction::Json(ui_tx) =
        &tx_data.transaction.transaction
        && let solana_transaction_status::UiMessage::Raw(raw) = &ui_tx.message
    {
        println!("\n{}", "━━ Instructions ━━".bold());
        for (i, ix) in raw.instructions.iter().enumerate() {
            let program_idx = ix.program_id_index as usize;
            let program_id = all_keys.get(program_idx);
            let program_name = program_id
                .and_then(|pk| known.get(pk))
                .map(|s| s.to_string())
                .unwrap_or_else(|| program_id.map_or("unknown".to_string(), |pk| pk.to_string()));

            let data_bytes = bs58::decode(&ix.data).into_vec().unwrap_or_default();
            let ix_name = instruction_name(&data_bytes).unwrap_or("unknown");

            println!(
                "\n[{}] {} {}",
                i.to_string().dimmed(),
                program_name.cyan(),
                ix_name.bold()
            );

            // Decode specific instructions
            if ix_name == "CallContract" && data_bytes.len() > 8 {
                if let Ok((dest_chain, rest)) = decode_borsh_string(&data_bytes[8..]) {
                    println!("    destination_chain: \"{}\"", dest_chain);
                    if let Ok((dest_addr, rest)) = decode_borsh_string(rest) {
                        println!("    destination_address: \"{}\"", dest_addr);
                        if rest.len() >= 32 {
                            println!("    payload_hash: {}", hex::encode(&rest[..32]));
                        }
                    }
                }
            } else if ix_name == "PayGas"
                && data_bytes.len() > 8
                && let Ok((dest_chain, rest)) = decode_borsh_string(&data_bytes[8..])
            {
                println!("    destination_chain: \"{}\"", dest_chain);
                if let Ok((dest_addr, rest)) = decode_borsh_string(rest) {
                    println!("    destination_address: \"{}\"", dest_addr);
                    if rest.len() >= 32 {
                        println!("    payload_hash: {}", hex::encode(&rest[..32]));
                    }
                }
            }

            // Print accounts
            for (j, &acc_idx) in ix.accounts.iter().enumerate() {
                let acc = all_keys.get(acc_idx as usize);
                let acc_str = acc.map_or("?".to_string(), |pk| {
                    let name = known.get(pk).map(|s| format!(" ({})", s));
                    format!("{}{}", pk, name.unwrap_or_default())
                });
                println!("    Account {}: {}", j.to_string().dimmed(), acc_str);
            }
        }
    }

    // Print inner instructions (CPI events)
    if let OptionSerializer::Some(inner_ixs) = &meta.inner_instructions {
        if !inner_ixs.is_empty() {
            println!("\n{}", "━━ Inner Instructions (CPI) ━━".bold());
        }
        for group in inner_ixs {
            for (i, ix) in group.instructions.iter().enumerate() {
                if let UiInstruction::Compiled(ci) = ix {
                    let program_id = all_keys.get(ci.program_id_index as usize);
                    let program_name = program_id
                        .and_then(|pk| known.get(pk))
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| {
                            program_id.map_or("?".to_string(), |pk| {
                                let s = pk.to_string();
                                if s.len() > 8 {
                                    format!("{}..{}", &s[..4], &s[s.len() - 4..])
                                } else {
                                    s
                                }
                            })
                        });

                    let data_bytes = bs58::decode(&ci.data).into_vec().unwrap_or_default();

                    // Check if this is an Anchor CPI event
                    if data_bytes.len() >= 16 && data_bytes[..8] == *EVENT_IX_TAG_LE {
                        let event_disc = &data_bytes[8..16];
                        let event = event_name(event_disc).unwrap_or("UnknownEvent");
                        println!(
                            "\n  [{}:{}] {} {} (depth {})",
                            group.index,
                            i,
                            program_name.cyan(),
                            format!("EVENT: {event}").yellow().bold(),
                            ci.stack_height.unwrap_or(0),
                        );

                        // Try to decode event data
                        if event == "CallContractEvent"
                            && let Some(decoded) = try_decode_call_contract_event(&data_bytes[16..])
                        {
                            for line in decoded.lines() {
                                println!("      {line}");
                            }
                        }
                    } else {
                        let ix_name = instruction_name(&data_bytes).unwrap_or("unknown");
                        println!(
                            "\n  [{}:{}] {} {} (depth {})",
                            group.index,
                            i,
                            program_name.cyan(),
                            ix_name.bold(),
                            ci.stack_height.unwrap_or(0),
                        );
                    }
                }
            }
        }
    }

    // Print logs
    if !logs.is_empty() {
        println!("\n{}", "━━ Logs ━━".bold());
        for log in &logs {
            if log.contains("invoke") {
                println!("  {}", log.dimmed());
            } else if log.contains("success") {
                println!("  {}", log.green());
            } else if log.contains("failed") || log.contains("Error") {
                println!("  {}", log.red());
            } else {
                println!("  {log}");
            }
        }
    }

    Ok(())
}

/// Decode a borsh-encoded string (4-byte little-endian length + UTF-8 bytes)
fn decode_borsh_string(data: &[u8]) -> Result<(String, &[u8])> {
    if data.len() < 4 {
        bail!("not enough data for string length");
    }
    let len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if data.len() < 4 + len {
        bail!("not enough data for string content");
    }
    let s = String::from_utf8_lossy(&data[4..4 + len]).to_string();
    Ok((s, &data[4 + len..]))
}
