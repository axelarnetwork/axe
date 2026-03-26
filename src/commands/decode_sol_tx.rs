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

/// Return human-readable account role labels for known instructions.
fn account_labels(ix_name: &str) -> &'static [&'static str] {
    match ix_name {
        "CallContract" => &[
            "sender",
            "sender",
            "gateway_root_pda",
            "event_authority",
            "gateway_program",
        ],
        "InitializePayloadVerificationSession" => &[
            "payer",
            "gateway_root_pda",
            "verification_session",
            "verifier_set_tracker",
            "system_program",
        ],
        "VerifySignature" => &[
            "gateway_root_pda",
            "verification_session",
            "verifier_set_tracker",
        ],
        "ApproveMessage" => &[
            "gateway_root_pda",
            "funder",
            "verification_session",
            "incoming_message",
            "system_program",
            "event_authority",
            "gateway_program",
        ],
        "ValidateMessage" => &["incoming_message", "caller", "gateway_root_pda"],
        "RotateSigners" => &[
            "payer",
            "gateway_root_pda",
            "verification_session",
            "new_verifier_set_tracker",
            "system_program",
            "event_authority",
            "gateway_program",
        ],
        "PayGas" => &[
            "sender",
            "gas_config_pda",
            "system_program",
            "event_authority",
            "gas_service_program",
        ],
        "SendMemo" => &[
            "memo_program",
            "sender",
            "gateway_root_pda",
            "event_authority",
            "gateway_program",
        ],
        "InterchainTransfer" => &[
            "payer",
            "its_root_pda",
            "token_manager",
            "source_account",
            "token_mint",
            "token_program",
            "gateway_root_pda",
            "gas_config_pda",
        ],
        "DeployInterchainToken" => &[
            "payer",
            "its_root_pda",
            "token_mint",
            "token_manager",
            "system_program",
        ],
        "DeployRemoteInterchainToken" => &[
            "payer",
            "its_root_pda",
            "token_manager",
            "gateway_root_pda",
            "gas_config_pda",
        ],
        _ => &[],
    }
}

/// Format an account pubkey with its known program name and role label.
fn format_account(
    pk: &Pubkey,
    known: &HashMap<Pubkey, &'static str>,
    label: Option<&str>,
) -> String {
    let name = known.get(pk).map(|s| format!(" ({})", s));
    let role = label.map(|l| format!(" ← {}", l.dimmed()));
    format!(
        "{}{}{}",
        pk,
        name.unwrap_or_default(),
        role.unwrap_or_default()
    )
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
        // Gateway events
        [0xd3, 0xd3, 0x50, 0x7e, 0x96, 0x62, 0xb5, 0xc6] => Some("CallContractEvent"),
        [0xfa, 0xfe, 0x1d, 0xe3, 0x9f, 0xcd, 0x72, 0x59] => Some("MessageApprovedEvent"),
        [0x09, 0x9d, 0xbc, 0xe1, 0xa8, 0x1a, 0x5e, 0x52] => Some("MessageExecutedEvent"),
        [0x36, 0x4f, 0x98, 0x9b, 0x8a, 0x44, 0xe5, 0x60] => Some("VerifierSetRotatedEvent"),
        // GasService events
        [0xbf, 0xa1, 0x16, 0xab, 0x29, 0x20, 0xd4, 0xf8] => Some("GasPaidEvent"),
        [0xa3, 0x3f, 0xa2, 0x06, 0x89, 0x81, 0x69, 0x6c] => Some("NativeGasPaidEvent"),
        [0x43, 0x61, 0xf5, 0x20, 0xc3, 0xb4, 0x4a, 0x6d] => Some("GasAddedEvent"),
        [0xa3, 0x95, 0x04, 0x86, 0xf5, 0x64, 0xca, 0x36] => Some("NativeGasAddedEvent"),
        [0xdc, 0x03, 0x99, 0xf4, 0x85, 0xbd, 0x49, 0x77] => Some("RefundedEvent"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Decode CallContractEvent from borsh
// ---------------------------------------------------------------------------

/// Try to decode the payload and return a human-readable summary.
/// Returns (summary_line, Option<decoded_content>)
fn decode_payload(payload: &[u8]) -> (String, Option<String>) {
    if payload.is_empty() {
        return ("empty".to_string(), None);
    }

    // Check if it's an Axelar encoded payload (first byte = encoding scheme)
    if matches!(payload[0], 0x00 | 0x01)
        && let Ok(decoded) = solana_axelar_gateway::payload::AxelarMessagePayload::decode(payload)
    {
        let inner = decoded.payload_without_accounts();

        // Try to decode the inner data
        let content = try_decode_payload_content(inner);
        let size = format!("{} bytes", payload.len());
        return (size, content);
    }

    // Raw payload — try to interpret as UTF-8
    if let Ok(s) = std::str::from_utf8(payload)
        && s.is_ascii()
        && !s.is_empty()
    {
        return (format!("{} bytes", payload.len()), Some(format!("\"{s}\"")));
    }

    (format!("{} bytes", payload.len()), None)
}

/// Try to decode inner payload data as a known type.
fn try_decode_payload_content(data: &[u8]) -> Option<String> {
    use alloy::dyn_abi::DynSolType;

    if data.is_empty() {
        return None;
    }

    // Try as ABI-encoded (string) — common for GMP memo payloads
    if let Ok(val) = DynSolType::String.abi_decode(data)
        && let Some(s) = val.as_str()
        && !s.is_empty()
    {
        return Some(format!("\"{s}\""));
    }

    // Try as ABI-encoded tuple(string) — SenderReceiver._execute format
    if let Ok(val) = DynSolType::Tuple(vec![DynSolType::String]).abi_decode(data)
        && let Some(vals) = val.as_tuple()
        && let Some(s) = vals.first().and_then(|v| v.as_str())
        && !s.is_empty()
    {
        return Some(format!("\"{s}\""));
    }

    // Try as raw UTF-8
    if let Ok(s) = std::str::from_utf8(data)
        && s.is_ascii()
        && !s.is_empty()
    {
        return Some(format!("\"{s}\""));
    }

    None
}

fn try_decode_call_contract_event(data: &[u8]) -> Option<String> {
    let event = borsh::from_slice::<solana_axelar_gateway::events::CallContractEvent>(data).ok()?;
    let (size, content) = decode_payload(&event.payload);
    let payload_line = match content {
        Some(decoded) => format!("{size} → {decoded}"),
        None => size,
    };
    Some(format!(
        "sender: {}\n      destination_chain: \"{}\"\n      destination_address: \"{}\"\n      payload_hash: {}\n      payload: {}",
        event.sender,
        event.destination_chain,
        event.destination_contract_address,
        hex::encode(event.payload_hash),
        payload_line,
    ))
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

const SOLANA_RPCS: &[(&str, &str)] = &[
    ("devnet", "https://api.devnet.solana.com"),
    ("testnet", "https://api.testnet.solana.com"),
    ("mainnet", "https://api.mainnet-beta.solana.com"),
];

pub async fn run(txid: &str, _solana_rpc: &str) -> Result<()> {
    let sig =
        Signature::from_str(txid).map_err(|e| eyre::eyre!("invalid Solana signature: {e}"))?;

    // Try all Solana networks to find the transaction
    let mut tx_data: Option<(String, EncodedConfirmedTransactionWithStatusMeta)> = None;

    for (network, rpc_url) in SOLANA_RPCS {
        let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
        if let Ok(data) = rpc.get_transaction_with_config(
            &sig,
            solana_client::rpc_config::RpcTransactionConfig {
                encoding: Some(UiTransactionEncoding::Json),
                commitment: Some(CommitmentConfig::confirmed()),
                max_supported_transaction_version: Some(0),
            },
        ) {
            tx_data = Some((network.to_string(), data));
            break;
        }
    }

    let (network, tx_data) = tx_data.ok_or_else(|| {
        eyre::eyre!("transaction not found on any Solana network (tried devnet, testnet, mainnet)")
    })?;

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
    println!("{} Solana {}", "Network:".bold(), network.cyan());
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
            let program_label = program_id.and_then(|pk| known.get(pk)).copied();
            let program_addr = program_id.map_or("unknown".to_string(), |pk| pk.to_string());

            let data_bytes = bs58::decode(&ix.data).into_vec().unwrap_or_default();
            let ix_name = instruction_name(&data_bytes).unwrap_or("unknown");

            if let Some(label) = program_label {
                println!(
                    "\n[{}] {} {} {}",
                    i.to_string().dimmed(),
                    label.cyan(),
                    ix_name.bold(),
                    format!("({})", program_addr).dimmed(),
                );
            } else {
                println!(
                    "\n[{}] {} {}",
                    i.to_string().dimmed(),
                    program_addr.cyan(),
                    ix_name.bold()
                );
            }

            // Decode instruction arguments
            decode_instruction_args(ix_name, &data_bytes, "  │ ");

            // Print accounts with role labels
            println!("  {}", "Accounts:".dimmed());
            let labels = account_labels(ix_name);
            for (j, &acc_idx) in ix.accounts.iter().enumerate() {
                let acc = all_keys.get(acc_idx as usize);
                let label = labels.get(j).copied();
                let acc_str = acc.map_or("?".to_string(), |pk| format_account(pk, &known, label));
                println!("  │ {}: {}", j.to_string().dimmed(), acc_str);
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

                        // Print inner instruction accounts with labels
                        let inner_labels = account_labels(ix_name);
                        for (j, &acc_idx) in ci.accounts.iter().enumerate() {
                            let acc = all_keys.get(acc_idx as usize);
                            let label = inner_labels.get(j).copied();
                            let acc_str =
                                acc.map_or("?".to_string(), |pk| format_account(pk, &known, label));
                            println!("      Account {}: {}", j.to_string().dimmed(), acc_str);
                        }
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

/// Decode instruction arguments based on the instruction name.
fn decode_instruction_args(ix_name: &str, data: &[u8], indent: &str) {
    if data.len() <= 8 {
        return;
    }
    let args = &data[8..]; // skip 8-byte discriminator

    match ix_name {
        "CallContract" => {
            if let Ok((dest_chain, rest)) = decode_borsh_string(args) {
                println!("{indent}destination_chain: \"{dest_chain}\"");
                if let Ok((dest_addr, rest)) = decode_borsh_string(rest) {
                    println!("{indent}destination_address: \"{dest_addr}\"");
                    // Remaining bytes are the payload (borsh-encoded Vec<u8>)
                    if let Ok((payload_str, _)) = decode_borsh_bytes(rest) {
                        let (size, content) = decode_payload(&payload_str);
                        match content {
                            Some(decoded) => println!("{indent}payload: {size} → {decoded}"),
                            None => println!("{indent}payload: {size}"),
                        }
                    }
                }
            }
        }
        "PayGas" => {
            if let Ok((dest_chain, rest)) = decode_borsh_string(args) {
                println!("{indent}destination_chain: \"{dest_chain}\"");
                if let Ok((dest_addr, rest)) = decode_borsh_string(rest) {
                    println!("{indent}destination_address: \"{dest_addr}\"");
                    // Next 32 bytes are the payload_hash
                    if rest.len() >= 32 {
                        println!("{indent}payload_hash: {}", hex::encode(&rest[..32]));
                        let rest = &rest[32..];
                        // Remaining: gas_amount (u64) + refund_address (pubkey)
                        if rest.len() >= 8 {
                            let gas = u64::from_le_bytes(rest[..8].try_into().unwrap_or_default());
                            println!("{indent}gas_amount: {gas} lamports");
                        }
                    }
                }
            }
        }
        "InitializePayloadVerificationSession" => {
            if args.len() >= 33 {
                println!("{indent}merkle_root: {}", hex::encode(&args[..32]));
                let payload_type = match args[32] {
                    0 => "ApproveMessages",
                    1 => "RotateSigners",
                    _ => "Unknown",
                };
                println!("{indent}payload_type: {payload_type}");
            }
        }
        "VerifySignature" => {
            if args.len() >= 32 {
                println!("{indent}payload_merkle_root: {}", hex::encode(&args[..32]));
                if args.len() > 32 {
                    println!("{indent}verifier_info: {} bytes", args.len() - 32);
                }
            }
        }
        "SendMemo" => {
            if let Ok((dest_chain, rest)) = decode_borsh_string(args) {
                println!("{indent}destination_chain: \"{dest_chain}\"");
                if let Ok((dest_addr, rest)) = decode_borsh_string(rest) {
                    println!("{indent}destination_address: \"{dest_addr}\"");
                    if let Ok((memo, _)) = decode_borsh_string(rest) {
                        println!("{indent}memo: \"{memo}\"");
                    }
                }
            }
        }
        _ => {
            if args.len() <= 64 {
                println!("{indent}data: {}", hex::encode(args));
            } else {
                println!("{indent}data: {} bytes", args.len());
            }
        }
    }
}

/// Decode borsh-encoded bytes (4-byte little-endian length + raw bytes)
fn decode_borsh_bytes(data: &[u8]) -> Result<(Vec<u8>, &[u8])> {
    if data.len() < 4 {
        bail!("not enough data for bytes length");
    }
    let len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if data.len() < 4 + len {
        bail!("not enough data for bytes content");
    }
    Ok((data[4..4 + len].to_vec(), &data[4 + len..]))
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
