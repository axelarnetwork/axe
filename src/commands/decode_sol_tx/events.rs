//! Anchor CPI event payload decoders. The `decode_anchor_event` dispatcher
//! routes a borsh-encoded body to the right typed decoder based on the
//! event name resolved from the discriminator.

use solana_sdk::pubkey::Pubkey;

use super::format::{format_address_bytes, kv_lines};
use super::parsing::{decode_borsh_bytes, decode_borsh_string, decode_payload};

/// Dispatch a borsh-encoded Anchor event payload to the right typed decoder
/// based on the event name. Returns `None` for events we don't know how to
/// pretty-print.
pub(super) fn decode_anchor_event(event: &str, body: &[u8]) -> Option<String> {
    match event {
        "CallContractEvent" => try_decode_call_contract_event(body),
        "InterchainTransferSentEvent" => try_decode_interchain_transfer_sent_event(body),
        "InterchainTransferReceivedEvent" => try_decode_interchain_transfer_received_event(body),
        "InterchainTokenDeployedEvent" => try_decode_interchain_token_deployed_event(body),
        "TokenManagerDeployedEvent" => try_decode_token_manager_deployed_event(body),
        "MessageApprovedEvent" => try_decode_message_approved_event(body),
        "MessageExecutedEvent" => try_decode_message_executed_event(body),
        "VerifierSetRotatedEvent" => try_decode_verifier_set_rotated_event(body),
        "GasPaidEvent" => try_decode_gas_paid_event(body),
        "GasAddedEvent" => try_decode_gas_added_event(body),
        "GasRefundedEvent" => try_decode_gas_refunded_event(body),
        "InterchainTokenDeploymentStartedEvent" => {
            try_decode_interchain_token_deployment_started_event(body)
        }
        _ => None,
    }
}

fn try_decode_interchain_transfer_sent_event(data: &[u8]) -> Option<String> {
    // token_id: [u8;32], source_address: Pubkey, source_token_account: Pubkey,
    // destination_chain: String, destination_address: Vec<u8>, amount: u64,
    // data_hash: Option<[u8;32]>
    if data.len() < 96 {
        return None;
    }
    let token_id = hex::encode(&data[..32]);
    let source = Pubkey::try_from(&data[32..64]).ok()?;
    let source_token = Pubkey::try_from(&data[64..96]).ok()?;
    let rest = &data[96..];
    let (dest_chain, rest) = decode_borsh_string(rest).ok()?;
    let (dest_addr_bytes, rest) = decode_borsh_bytes(rest).ok()?;
    let dest_addr = format_address_bytes(&dest_addr_bytes);
    if rest.len() < 8 {
        return None;
    }
    let amount = u64::from_le_bytes(rest[..8].try_into().ok()?);

    Some(kv_lines([
        ("token_id", token_id),
        ("source", source.to_string()),
        ("source_token_account", source_token.to_string()),
        ("destination_chain", format!("\"{dest_chain}\"")),
        ("destination_address", dest_addr),
        ("amount", amount.to_string()),
    ]))
}

fn try_decode_interchain_transfer_received_event(data: &[u8]) -> Option<String> {
    // command_id: [u8;32], token_id: [u8;32], source_chain: String,
    // source_address: Vec<u8>, destination_address: Pubkey,
    // destination_token_account: Pubkey, amount: u64, data_hash: Option<[u8;32]>
    if data.len() < 64 {
        return None;
    }
    let command_id = hex::encode(&data[..32]);
    let token_id = hex::encode(&data[32..64]);
    let rest = &data[64..];
    let (source_chain, rest) = decode_borsh_string(rest).ok()?;
    let (source_addr_bytes, rest) = decode_borsh_bytes(rest).ok()?;
    let source_addr = hex::encode(&source_addr_bytes);
    if rest.len() < 72 {
        return None;
    }
    let dest = Pubkey::try_from(&rest[..32]).ok()?;
    let dest_token = Pubkey::try_from(&rest[32..64]).ok()?;
    let amount = u64::from_le_bytes(rest[64..72].try_into().ok()?);

    Some(kv_lines([
        ("command_id", command_id),
        ("token_id", token_id),
        ("source_chain", format!("\"{source_chain}\"")),
        ("source_address", format!("0x{source_addr}")),
        ("destination", dest.to_string()),
        ("destination_token_account", dest_token.to_string()),
        ("amount", amount.to_string()),
    ]))
}

fn try_decode_interchain_token_deployed_event(data: &[u8]) -> Option<String> {
    // token_id: [u8;32], token_address: Pubkey, name: String, symbol: String,
    // decimals: u8, minter: Option<Pubkey>
    if data.len() < 64 {
        return None;
    }
    let token_id = hex::encode(&data[..32]);
    let token_address = Pubkey::try_from(&data[32..64]).ok()?;
    let rest = &data[64..];
    let (name, rest) = decode_borsh_string(rest).ok()?;
    let (symbol, rest) = decode_borsh_string(rest).ok()?;
    if rest.is_empty() {
        return None;
    }
    let decimals = rest[0];

    Some(kv_lines([
        ("token_id", token_id),
        ("token_address", token_address.to_string()),
        ("name", format!("\"{name}\"")),
        ("symbol", format!("\"{symbol}\"")),
        ("decimals", decimals.to_string()),
    ]))
}

fn try_decode_token_manager_deployed_event(data: &[u8]) -> Option<String> {
    // token_id: [u8;32], token_manager: Pubkey, token_manager_type: u8, params: Option<Vec<u8>>
    if data.len() < 65 {
        return None;
    }
    let token_id = hex::encode(&data[..32]);
    let token_manager = Pubkey::try_from(&data[32..64]).ok()?;
    let tm_type = data[64];
    let tm_type_str = match tm_type {
        0 => "NativeInterchainToken",
        1 => "MintBurnFrom",
        2 => "LockUnlock",
        3 => "LockUnlockFee",
        4 => "MintBurn",
        5 => "Gateway",
        _ => "Unknown",
    };

    Some(kv_lines([
        ("token_id", token_id),
        ("token_manager", token_manager.to_string()),
        ("type", tm_type_str.to_string()),
    ]))
}

fn try_decode_message_approved_event(data: &[u8]) -> Option<String> {
    // command_id: [u8;32], destination_address: String, payload_hash: [u8;32],
    // source_chain: String, cc_id: String, source_address: String, destination_chain: String
    if data.len() < 32 {
        return None;
    }
    let command_id = hex::encode(&data[..32]);
    let rest = &data[32..];
    let (dest_addr, rest) = decode_borsh_string(rest).ok()?;
    if rest.len() < 32 {
        return None;
    }
    let payload_hash = hex::encode(&rest[..32]);
    let rest = &rest[32..];
    let (source_chain, rest) = decode_borsh_string(rest).ok()?;
    let (cc_id, rest) = decode_borsh_string(rest).ok()?;
    let (source_addr, _rest) = decode_borsh_string(rest).ok()?;
    Some(kv_lines([
        ("command_id", command_id),
        ("source_chain", format!("\"{source_chain}\"")),
        ("cc_id", format!("\"{cc_id}\"")),
        ("source_address", format!("\"{source_addr}\"")),
        ("destination_address", format!("\"{dest_addr}\"")),
        ("payload_hash", payload_hash),
    ]))
}

fn try_decode_message_executed_event(data: &[u8]) -> Option<String> {
    // command_id: [u8;32], destination_address: Pubkey, payload_hash: [u8;32],
    // source_chain: String, cc_id: String, source_address: String, destination_chain: String
    if data.len() < 96 {
        return None;
    }
    let command_id = hex::encode(&data[..32]);
    let dest = Pubkey::try_from(&data[32..64]).ok()?;
    let payload_hash = hex::encode(&data[64..96]);
    let rest = &data[96..];
    let (source_chain, rest) = decode_borsh_string(rest).ok()?;
    let (cc_id, rest) = decode_borsh_string(rest).ok()?;
    let (source_addr, _rest) = decode_borsh_string(rest).ok()?;
    Some(kv_lines([
        ("command_id", command_id),
        ("source_chain", format!("\"{source_chain}\"")),
        ("cc_id", format!("\"{cc_id}\"")),
        ("source_address", format!("\"{source_addr}\"")),
        ("destination", dest.to_string()),
        ("payload_hash", payload_hash),
    ]))
}

fn try_decode_verifier_set_rotated_event(data: &[u8]) -> Option<String> {
    // epoch: U256 (32 bytes LE), verifier_set_hash: [u8;32]
    if data.len() < 64 {
        return None;
    }
    // U256 as little-endian, read as u64 for display (epochs are small)
    let epoch = u64::from_le_bytes(data[..8].try_into().ok()?);
    let verifier_set_hash = hex::encode(&data[32..64]);
    Some(kv_lines([
        ("epoch", epoch.to_string()),
        ("verifier_set_hash", verifier_set_hash),
    ]))
}

fn try_decode_gas_paid_event(data: &[u8]) -> Option<String> {
    // sender: Pubkey, destination_chain: String, destination_address: String,
    // payload_hash: [u8;32], amount: u64, refund_address: Pubkey
    if data.len() < 32 {
        return None;
    }
    let sender = Pubkey::try_from(&data[..32]).ok()?;
    let rest = &data[32..];
    let (dest_chain, rest) = decode_borsh_string(rest).ok()?;
    let (dest_addr, rest) = decode_borsh_string(rest).ok()?;
    if rest.len() < 72 {
        return None;
    }
    let payload_hash = hex::encode(&rest[..32]);
    let amount = u64::from_le_bytes(rest[32..40].try_into().ok()?);
    let refund = Pubkey::try_from(&rest[40..72]).ok()?;
    Some(kv_lines([
        ("sender", sender.to_string()),
        ("destination_chain", format!("\"{dest_chain}\"")),
        ("destination_address", format!("\"{dest_addr}\"")),
        ("payload_hash", payload_hash),
        ("amount", format!("{amount} lamports")),
        ("refund_address", refund.to_string()),
    ]))
}

fn try_decode_gas_added_event(data: &[u8]) -> Option<String> {
    // sender: Pubkey, message_id: String, amount: u64, refund_address: Pubkey
    if data.len() < 32 {
        return None;
    }
    let sender = Pubkey::try_from(&data[..32]).ok()?;
    let rest = &data[32..];
    let (message_id, rest) = decode_borsh_string(rest).ok()?;
    if rest.len() < 40 {
        return None;
    }
    let amount = u64::from_le_bytes(rest[..8].try_into().ok()?);
    let refund = Pubkey::try_from(&rest[8..40]).ok()?;
    Some(kv_lines([
        ("sender", sender.to_string()),
        ("message_id", format!("\"{message_id}\"")),
        ("amount", format!("{amount} lamports")),
        ("refund_address", refund.to_string()),
    ]))
}

fn try_decode_gas_refunded_event(data: &[u8]) -> Option<String> {
    // receiver: Pubkey, message_id: String, amount: u64
    if data.len() < 32 {
        return None;
    }
    let receiver = Pubkey::try_from(&data[..32]).ok()?;
    let rest = &data[32..];
    let (message_id, rest) = decode_borsh_string(rest).ok()?;
    if rest.len() < 8 {
        return None;
    }
    let amount = u64::from_le_bytes(rest[..8].try_into().ok()?);
    Some(kv_lines([
        ("receiver", receiver.to_string()),
        ("message_id", format!("\"{message_id}\"")),
        ("amount", format!("{amount} lamports")),
    ]))
}

fn try_decode_interchain_token_deployment_started_event(data: &[u8]) -> Option<String> {
    // token_id: [u8;32], destination_chain: String, name: String, symbol: String,
    // decimals: u8, minter: Option<Pubkey>
    if data.len() < 32 {
        return None;
    }
    let token_id = hex::encode(&data[..32]);
    let rest = &data[32..];
    let (dest_chain, rest) = decode_borsh_string(rest).ok()?;
    let (name, rest) = decode_borsh_string(rest).ok()?;
    let (symbol, rest) = decode_borsh_string(rest).ok()?;
    if rest.is_empty() {
        return None;
    }
    let decimals = rest[0];
    Some(kv_lines([
        ("token_id", token_id),
        ("destination_chain", format!("\"{dest_chain}\"")),
        ("name", format!("\"{name}\"")),
        ("symbol", format!("\"{symbol}\"")),
        ("decimals", decimals.to_string()),
    ]))
}

fn try_decode_call_contract_event(data: &[u8]) -> Option<String> {
    let event = borsh::from_slice::<solana_axelar_gateway::events::CallContractEvent>(data).ok()?;
    let (size, content) = decode_payload(&event.payload);
    let payload_line = match content {
        Some(decoded) => format!("{size} → {decoded}"),
        None => size,
    };
    Some(kv_lines([
        ("sender", event.sender.to_string()),
        (
            "destination_chain",
            format!("\"{}\"", event.destination_chain),
        ),
        (
            "destination_address",
            format!("\"{}\"", event.destination_contract_address),
        ),
        ("payload_hash", hex::encode(event.payload_hash)),
        ("payload", payload_line),
    ]))
}
