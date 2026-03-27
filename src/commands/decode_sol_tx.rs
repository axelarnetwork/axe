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

    // Compiled feature's IDs (always included)
    m.insert(solana_axelar_gateway::id(), "AxelarGateway");
    m.insert(solana_axelar_gas_service::id(), "AxelarGasService");
    m.insert(solana_axelar_memo::id(), "AxelarMemo");
    m.insert(solana_axelar_its::id(), "AxelarITS");

    // All known program IDs across all networks so decode works regardless of build feature
    let all_ids: &[(&str, &str)] = &[
        // devnet-amplifier
        (
            "gtwT4uGVTYSPnTGv6rSpMheyFyczUicxVWKqdtxNGw9",
            "AxelarGateway",
        ),
        (
            "gasHyxjNZSNsEiMbRLa5JGLCNx1TRsdCy1xwfMBehYB",
            "AxelarGasService",
        ),
        ("memKnP9ex71TveNFpsFNVqAYGEe1v9uHVsHNdFPW6FY", "AxelarMemo"),
        ("itsm3zZhp2oGgEfq7XBu9ojRCYZJnhzecbAEPCrvx2B", "AxelarITS"),
        // stagenet
        (
            "gtwYHfHHipAoj8Hfp3cGr3vhZ8f3UtptGCQLqjBkaSZ",
            "AxelarGateway",
        ),
        (
            "gasgy6jz24wrfZL98uMy8QFUFziVPZ3bNLGXqnyTstW",
            "AxelarGasService",
        ),
        ("mem4E22pPgkbHAvoUYHa7HybBgUKn6jFjvj1YnPdkaq", "AxelarMemo"),
        ("itsm3zZhp2oGgEfq7XBu9ojRCYZJnhzecbAEPCrvx2B", "AxelarITS"),
        // testnet
        (
            "gtwJ8LWDRWZpbvCqp8sDeTgy3GSyuoEsiaKC8wSXJqq",
            "AxelarGateway",
        ),
        (
            "gasq7KHHv9Rs8C82hu3dgoBD9wk5LTKpWqbdf5o5juu",
            "AxelarGasService",
        ),
        ("mem7UJouaeyTgySvXhQSxWtGFrWPQ89jywjc8YvQFRT", "AxelarMemo"),
        ("itsJo4kNJ3mdh3requwbtTTt7vyYTudp1pxhn2KiHMc", "AxelarITS"),
        // older devnet deployments
        ("itsYxmqAxNKUL5zaj3fD1K1whuVhqpxKVoiLGie1reF", "AxelarITS"),
        // Well-known system programs
        ("11111111111111111111111111111111", "SystemProgram"),
        (
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
            "TokenProgram",
        ),
        ("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb", "Token2022"),
        (
            "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
            "AssociatedToken",
        ),
        (
            "metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s",
            "MetaplexMetadata",
        ),
        (
            "Sysvar1nstructions1111111111111111111111111",
            "SysvarInstructions",
        ),
        (
            "ComputeBudget111111111111111111111111111111",
            "ComputeBudget",
        ),
        ("SysvarRent111111111111111111111111111111111", "SysvarRent"),
    ];

    for (addr, name) in all_ids {
        if let Ok(pk) = addr.parse() {
            m.entry(pk).or_insert(name);
        }
    }

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
        [0x82, 0xdd, 0xf2, 0x9a, 0x0d, 0xc1, 0xbd, 0x1d] => Some("Execute"),
        [0x36, 0xa7, 0x65, 0x49, 0xb0, 0xbd, 0x78, 0x41] => Some("ExecuteInterchainTransfer"),
        [0xf3, 0x9b, 0x31, 0xbd, 0x5e, 0xea, 0xb2, 0x69] => Some("ExecuteDeployInterchainToken"),
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
        "ValidateMessage" => &[
            "incoming_message_pda",
            "caller",
            "gateway_root_pda",
            "event_authority",
            "gateway_program",
        ],
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
            "authority",
            "gateway_root_pda",
            "gateway_event_authority",
            "gateway_program",
            "call_contract_signing_pda",
            "gas_treasury",
            "gas_service",
            "gas_event_authority",
            "its_root_pda",
            "token_manager_pda",
            "token_program",
            "token_mint",
            "authority_token_account",
            "token_manager_ata",
            "system_program",
            "its_event_authority",
            "its_program",
        ],
        "Execute" => &[
            "incoming_message_pda",
            "signing_pda",
            "gateway_root_pda",
            "gateway_event_authority",
            "gateway_program",
            "payer",
            "its_root_pda",
            "token_manager_pda",
            "token_mint",
            "token_manager_ata",
            "token_program",
            "associated_token_program",
            "system_program",
            "its_event_authority",
            "its_program",
            // remaining_accounts for deploy/transfer
            "sysvar_instructions",
            "mpl_token_metadata_program",
            "mpl_token_metadata_account",
        ],
        // CPI accounts are reordered by Anchor's to_account_metas
        // (writable signers, readonly signers, writable, readonly)
        "ExecuteDeployInterchainToken" => &[
            "payer",
            "system_program",
            "its_root_pda",
            "token_manager_pda",
            "token_mint",
            "token_manager_ata",
            "token_program",
            "associated_token_program",
            "sysvar_instructions",
            "mpl_token_metadata_program",
            "mpl_token_metadata_account",
            "its_event_authority",
            "its_program",
            "minter",
            "minter_roles_pda",
        ],
        "ExecuteInterchainTransfer" => &[
            "payer",
            "its_root_pda",
            "destination",
            "destination_ata",
            "token_mint",
            "token_manager_pda",
            "token_manager_ata",
            "token_program",
            "associated_token_program",
            "system_program",
            "its_program",
            "its_event_authority",
            "its_program",
        ],
        "DeployInterchainToken" => &[
            "payer",
            "deployer",
            "system_program",
            "its_root_pda",
            "token_manager_pda",
            "token_mint",
            "token_manager_ata",
            "token_program",
            "associated_token_program",
            "sysvar_instructions",
            "mpl_token_metadata_program",
            "mpl_token_metadata_account",
            "deployer_ata",
            "minter",
            "minter_roles_pda",
        ],
        "DeployRemoteInterchainToken" => &[
            "payer",
            "deployer",
            "token_mint",
            "metadata_account",
            "token_manager_pda",
            "gateway_root_pda",
            "gateway_program",
            "system_program",
            "its_root_pda",
            "call_contract_signing_pda",
            "gateway_event_authority",
            "gas_treasury",
            "gas_service",
            "gas_event_authority",
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
        [0x43, 0x61, 0xf5, 0x20, 0xc3, 0xb4, 0x4a, 0x6d] => Some("GasAddedEvent"),
        [0xea, 0xd0, 0x71, 0x56, 0x5d, 0x7b, 0xc8, 0x0c] => Some("GasRefundedEvent"),
        [0x29, 0x99, 0xc7, 0x9f, 0x73, 0x4b, 0x4c, 0xb8] => Some("GasCollectedEvent"),
        // ITS events
        [0x60, 0x42, 0x01, 0x44, 0xf0, 0x34, 0x90, 0x8a] => Some("InterchainTransferSentEvent"),
        [0xaf, 0xc9, 0xb2, 0x8b, 0x99, 0x45, 0x01, 0xd0] => Some("InterchainTransferReceivedEvent"),
        [0xf9, 0x5a, 0x7c, 0x8e, 0x42, 0x2a, 0x5c, 0xbc] => Some("InterchainTokenDeployedEvent"),
        [0x91, 0x4a, 0xc7, 0xba, 0xd2, 0xe8, 0x93, 0x01] => {
            Some("InterchainTokenDeploymentStartedEvent")
        }
        [0x03, 0x8a, 0x01, 0x9b, 0x81, 0x57, 0x00, 0x29] => Some("TokenManagerDeployedEvent"),
        [0x1b, 0x1f, 0xbd, 0xfb, 0xb7, 0x29, 0x08, 0x7c] => Some("TokenMetadataRegisteredEvent"),
        [0xef, 0x48, 0x83, 0xb5, 0xfb, 0x01, 0xde, 0x82] => Some("LinkTokenStartedEvent"),
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

    // Try decoding directly (e.g. ITS HubMessage is Borsh without AxelarMessagePayload wrapper)
    if let Some(content) = try_decode_payload_content(payload) {
        return (format!("{} bytes", payload.len()), Some(content));
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

    // Try as ITS HubMessage (Borsh-encoded)
    if let Some(s) = try_decode_its_hub_message(data) {
        return Some(s);
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

fn try_decode_its_hub_message(data: &[u8]) -> Option<String> {
    use solana_axelar_its::encoding::HubMessage;

    let hub_msg: HubMessage = borsh::from_slice(data).ok()?;
    match hub_msg {
        HubMessage::SendToHub {
            destination_chain,
            message,
        } => {
            let mut lines = vec![format!("ITS SendToHub → \"{destination_chain}\"")];
            format_its_message(&message, &mut lines);
            Some(lines.join("\n"))
        }
        HubMessage::ReceiveFromHub {
            source_chain,
            message,
        } => {
            let mut lines = vec![format!("ITS ReceiveFromHub ← \"{source_chain}\"")];
            format_its_message(&message, &mut lines);
            Some(lines.join("\n"))
        }
        HubMessage::RegisterTokenMetadata(_) => Some("ITS RegisterTokenMetadata".to_string()),
    }
}

fn format_its_message(message: &solana_axelar_its::encoding::Message, lines: &mut Vec<String>) {
    use owo_colors::OwoColorize;
    use solana_axelar_its::encoding::Message;

    let p = "      ┃ ";
    match message {
        Message::InterchainTransfer(t) => {
            lines.push(format!("      ┏━ {}", "InterchainTransfer".bold()));
            lines.push(format!(
                "{p}{} {}",
                "token_id:".dimmed(),
                hex::encode(t.token_id)
            ));
            lines.push(format!(
                "{p}{} {}",
                "source:".dimmed(),
                format_address_bytes(&t.source_address)
            ));
            lines.push(format!(
                "{p}{} {}",
                "destination:".dimmed(),
                format_address_bytes(&t.destination_address)
            ));
            lines.push(format!("{p}{} {}", "amount:".dimmed(), t.amount));
            lines.push("      ┗━".to_string());
        }
        Message::DeployInterchainToken(t) => {
            lines.push(format!("      ┏━ {}", "DeployInterchainToken".bold()));
            lines.push(format!(
                "{p}{} {}",
                "token_id:".dimmed(),
                hex::encode(t.token_id)
            ));
            lines.push(format!("{p}{} \"{}\"", "name:".dimmed(), t.name));
            lines.push(format!("{p}{} \"{}\"", "symbol:".dimmed(), t.symbol));
            lines.push(format!("{p}{} {}", "decimals:".dimmed(), t.decimals));
            lines.push("      ┗━".to_string());
        }
        Message::LinkToken(t) => {
            lines.push(format!("      ┏━ {}", "LinkToken".bold()));
            lines.push(format!(
                "{p}{} {}",
                "token_id:".dimmed(),
                hex::encode(t.token_id)
            ));
            lines.push(format!(
                "{p}{} {}",
                "token_manager_type:".dimmed(),
                t.token_manager_type
            ));
            lines.push(format!(
                "{p}{} {}",
                "source_token:".dimmed(),
                format_address_bytes(&t.source_token_address)
            ));
            lines.push(format!(
                "{p}{} {}",
                "destination_token:".dimmed(),
                format_address_bytes(&t.destination_token_address)
            ));
            lines.push("      ┗━".to_string());
        }
    }
}

/// Format raw address bytes for display: 20 bytes → EVM 0x,
/// 32 bytes → Solana base58, valid ASCII → string, otherwise → 0x hex.
fn format_address_bytes(bytes: &[u8]) -> String {
    if bytes.len() == 20 {
        return format!("0x{}", hex::encode(bytes));
    }
    if bytes.len() == 32
        && let Ok(pk) = Pubkey::try_from(bytes)
    {
        return pk.to_string();
    }
    if let Ok(s) = std::str::from_utf8(bytes)
        && s.is_ascii()
        && !s.is_empty()
    {
        return s.to_string();
    }
    format!("0x{}", hex::encode(bytes))
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

    use owo_colors::OwoColorize;
    Some(format!(
        "{} {token_id}\n{} {source}\n{} {source_token}\n{} \"{dest_chain}\"\n{} {dest_addr}\n{} {amount}",
        "token_id:".dimmed(),
        "source:".dimmed(),
        "source_token_account:".dimmed(),
        "destination_chain:".dimmed(),
        "destination_address:".dimmed(),
        "amount:".dimmed(),
    ))
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

    use owo_colors::OwoColorize;
    Some(format!(
        "{} {command_id}\n{} {token_id}\n{} \"{source_chain}\"\n{} 0x{source_addr}\n{} {dest}\n{} {dest_token}\n{} {amount}",
        "command_id:".dimmed(),
        "token_id:".dimmed(),
        "source_chain:".dimmed(),
        "source_address:".dimmed(),
        "destination:".dimmed(),
        "destination_token_account:".dimmed(),
        "amount:".dimmed(),
    ))
}

fn try_decode_interchain_token_deployed_event(data: &[u8]) -> Option<String> {
    // token_id: [u8;32], token_address: Pubkey, name: String, symbol: String,
    // decimals: u8, minter: Option<Pubkey>
    use owo_colors::OwoColorize;
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

    Some(format!(
        "{} {token_id}\n{} {token_address}\n{} \"{name}\"\n{} \"{symbol}\"\n{} {decimals}",
        "token_id:".dimmed(),
        "token_address:".dimmed(),
        "name:".dimmed(),
        "symbol:".dimmed(),
        "decimals:".dimmed(),
    ))
}

fn try_decode_token_manager_deployed_event(data: &[u8]) -> Option<String> {
    // token_id: [u8;32], token_manager: Pubkey, token_manager_type: u8, params: Option<Vec<u8>>
    use owo_colors::OwoColorize;
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

    Some(format!(
        "{} {token_id}\n{} {token_manager}\n{} {tm_type_str}",
        "token_id:".dimmed(),
        "token_manager:".dimmed(),
        "type:".dimmed(),
    ))
}

fn try_decode_message_approved_event(data: &[u8]) -> Option<String> {
    // command_id: [u8;32], destination_address: String, payload_hash: [u8;32],
    // source_chain: String, cc_id: String, source_address: String, destination_chain: String
    use owo_colors::OwoColorize;
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
    Some(format!(
        "{} {command_id}\n{} \"{source_chain}\"\n{} \"{cc_id}\"\n{} \"{source_addr}\"\n{} \"{dest_addr}\"\n{} {payload_hash}",
        "command_id:".dimmed(),
        "source_chain:".dimmed(),
        "cc_id:".dimmed(),
        "source_address:".dimmed(),
        "destination_address:".dimmed(),
        "payload_hash:".dimmed(),
    ))
}

fn try_decode_message_executed_event(data: &[u8]) -> Option<String> {
    // command_id: [u8;32], destination_address: Pubkey, payload_hash: [u8;32],
    // source_chain: String, cc_id: String, source_address: String, destination_chain: String
    use owo_colors::OwoColorize;
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
    Some(format!(
        "{} {command_id}\n{} \"{source_chain}\"\n{} \"{cc_id}\"\n{} \"{source_addr}\"\n{} {dest}\n{} {payload_hash}",
        "command_id:".dimmed(),
        "source_chain:".dimmed(),
        "cc_id:".dimmed(),
        "source_address:".dimmed(),
        "destination:".dimmed(),
        "payload_hash:".dimmed(),
    ))
}

fn try_decode_verifier_set_rotated_event(data: &[u8]) -> Option<String> {
    // epoch: U256 (32 bytes LE), verifier_set_hash: [u8;32]
    use owo_colors::OwoColorize;
    if data.len() < 64 {
        return None;
    }
    // U256 as little-endian, read as u64 for display (epochs are small)
    let epoch = u64::from_le_bytes(data[..8].try_into().ok()?);
    let verifier_set_hash = hex::encode(&data[32..64]);
    Some(format!(
        "{} {epoch}\n{} {verifier_set_hash}",
        "epoch:".dimmed(),
        "verifier_set_hash:".dimmed(),
    ))
}

fn try_decode_gas_paid_event(data: &[u8]) -> Option<String> {
    // sender: Pubkey, destination_chain: String, destination_address: String,
    // payload_hash: [u8;32], amount: u64, refund_address: Pubkey
    use owo_colors::OwoColorize;
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
    Some(format!(
        "{} {sender}\n{} \"{dest_chain}\"\n{} \"{dest_addr}\"\n{} {payload_hash}\n{} {amount} lamports\n{} {refund}",
        "sender:".dimmed(),
        "destination_chain:".dimmed(),
        "destination_address:".dimmed(),
        "payload_hash:".dimmed(),
        "amount:".dimmed(),
        "refund_address:".dimmed(),
    ))
}

fn try_decode_gas_added_event(data: &[u8]) -> Option<String> {
    // sender: Pubkey, message_id: String, amount: u64, refund_address: Pubkey
    use owo_colors::OwoColorize;
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
    Some(format!(
        "{} {sender}\n{} \"{message_id}\"\n{} {amount} lamports\n{} {refund}",
        "sender:".dimmed(),
        "message_id:".dimmed(),
        "amount:".dimmed(),
        "refund_address:".dimmed(),
    ))
}

fn try_decode_gas_refunded_event(data: &[u8]) -> Option<String> {
    // receiver: Pubkey, message_id: String, amount: u64
    use owo_colors::OwoColorize;
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
    Some(format!(
        "{} {receiver}\n{} \"{message_id}\"\n{} {amount} lamports",
        "receiver:".dimmed(),
        "message_id:".dimmed(),
        "amount:".dimmed(),
    ))
}

fn try_decode_interchain_token_deployment_started_event(data: &[u8]) -> Option<String> {
    // token_id: [u8;32], destination_chain: String, name: String, symbol: String,
    // decimals: u8, minter: Option<Pubkey>
    use owo_colors::OwoColorize;
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
    Some(format!(
        "{} {token_id}\n{} \"{dest_chain}\"\n{} \"{name}\"\n{} \"{symbol}\"\n{} {decimals}",
        "token_id:".dimmed(),
        "destination_chain:".dimmed(),
        "name:".dimmed(),
        "symbol:".dimmed(),
        "decimals:".dimmed(),
    ))
}

fn try_decode_call_contract_event(data: &[u8]) -> Option<String> {
    use owo_colors::OwoColorize;
    let event = borsh::from_slice::<solana_axelar_gateway::events::CallContractEvent>(data).ok()?;
    let (size, content) = decode_payload(&event.payload);
    let payload_line = match content {
        Some(decoded) => format!("{size} → {decoded}"),
        None => size,
    };
    Some(format!(
        "{} {}\n{} \"{}\"\n{} \"{}\"\n{} {}\n{} {}",
        "sender:".dimmed(),
        event.sender,
        "destination_chain:".dimmed(),
        event.destination_chain,
        "destination_address:".dimmed(),
        event.destination_contract_address,
        "payload_hash:".dimmed(),
        hex::encode(event.payload_hash),
        "payload:".dimmed(),
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

            // Skip ComputeBudget instructions (noise)
            if program_label == Some("ComputeBudget") {
                continue;
            }

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

                    // Skip noisy system/token plumbing CPI calls
                    let is_system_program = matches!(
                        program_name.as_str(),
                        "SystemProgram"
                            | "Token2022"
                            | "TokenProgram"
                            | "AssociatedToken"
                            | "MetaplexMetadata"
                            | "ComputeBudget"
                    );
                    let is_event = data_bytes.len() >= 16 && data_bytes[..8] == *EVENT_IX_TAG_LE;

                    if is_system_program && !is_event {
                        continue;
                    }

                    // Check if this is an Anchor CPI event
                    if is_event {
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
                        let decoded = match event {
                            "CallContractEvent" => {
                                try_decode_call_contract_event(&data_bytes[16..])
                            }
                            "InterchainTransferSentEvent" => {
                                try_decode_interchain_transfer_sent_event(&data_bytes[16..])
                            }
                            "InterchainTransferReceivedEvent" => {
                                try_decode_interchain_transfer_received_event(&data_bytes[16..])
                            }
                            "InterchainTokenDeployedEvent" => {
                                try_decode_interchain_token_deployed_event(&data_bytes[16..])
                            }
                            "TokenManagerDeployedEvent" => {
                                try_decode_token_manager_deployed_event(&data_bytes[16..])
                            }
                            "MessageApprovedEvent" => {
                                try_decode_message_approved_event(&data_bytes[16..])
                            }
                            "MessageExecutedEvent" => {
                                try_decode_message_executed_event(&data_bytes[16..])
                            }
                            "VerifierSetRotatedEvent" => {
                                try_decode_verifier_set_rotated_event(&data_bytes[16..])
                            }
                            "GasPaidEvent" => try_decode_gas_paid_event(&data_bytes[16..]),
                            "GasAddedEvent" => try_decode_gas_added_event(&data_bytes[16..]),
                            "GasRefundedEvent" => try_decode_gas_refunded_event(&data_bytes[16..]),
                            "InterchainTokenDeploymentStartedEvent" => {
                                try_decode_interchain_token_deployment_started_event(
                                    &data_bytes[16..],
                                )
                            }
                            _ => None,
                        };
                        if let Some(decoded) = decoded {
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

                        // Decode instruction arguments
                        decode_instruction_args(ix_name, &data_bytes, "    │ ");

                        // Print inner instruction accounts with labels
                        println!("    {}", "Accounts:".dimmed());
                        let inner_labels = account_labels(ix_name);
                        for (j, &acc_idx) in ci.accounts.iter().enumerate() {
                            let acc = all_keys.get(acc_idx as usize);
                            let label = inner_labels.get(j).copied();
                            let acc_str =
                                acc.map_or("?".to_string(), |pk| format_account(pk, &known, label));
                            println!("    │ {}: {}", j.to_string().dimmed(), acc_str);
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Decode instruction arguments based on the instruction name.
fn decode_instruction_args(ix_name: &str, data: &[u8], indent: &str) {
    use owo_colors::OwoColorize;

    if data.len() <= 8 {
        return;
    }
    let args = &data[8..]; // skip 8-byte discriminator

    match ix_name {
        "CallContract" => {
            if let Ok((dest_chain, rest)) = decode_borsh_string(args) {
                println!("{indent}{} \"{dest_chain}\"", "destination_chain:".dimmed());
                if let Ok((dest_addr, rest)) = decode_borsh_string(rest) {
                    println!(
                        "{indent}{} \"{dest_addr}\"",
                        "destination_address:".dimmed()
                    );
                    if let Ok((payload_str, _)) = decode_borsh_bytes(rest) {
                        let (size, content) = decode_payload(&payload_str);
                        match content {
                            Some(decoded) => {
                                println!("{indent}{} {size} → {decoded}", "payload:".dimmed())
                            }
                            None => println!("{indent}{} {size}", "payload:".dimmed()),
                        }
                    }
                }
            }
        }
        "PayGas" => {
            if let Ok((dest_chain, rest)) = decode_borsh_string(args) {
                println!("{indent}{} \"{dest_chain}\"", "destination_chain:".dimmed());
                if let Ok((dest_addr, rest)) = decode_borsh_string(rest) {
                    println!(
                        "{indent}{} \"{dest_addr}\"",
                        "destination_address:".dimmed()
                    );
                    if rest.len() >= 32 {
                        println!(
                            "{indent}{} {}",
                            "payload_hash:".dimmed(),
                            hex::encode(&rest[..32])
                        );
                        let rest = &rest[32..];
                        if rest.len() >= 8 {
                            let gas = u64::from_le_bytes(rest[..8].try_into().unwrap_or_default());
                            println!("{indent}{} {gas} lamports", "gas_amount:".dimmed());
                        }
                    }
                }
            }
        }
        "InitializePayloadVerificationSession" => {
            if args.len() >= 33 {
                println!(
                    "{indent}{} {}",
                    "merkle_root:".dimmed(),
                    hex::encode(&args[..32])
                );
                let payload_type = match args[32] {
                    0 => "ApproveMessages",
                    1 => "RotateSigners",
                    _ => "Unknown",
                };
                println!("{indent}{} {payload_type}", "payload_type:".dimmed());
            }
        }
        "VerifySignature" => {
            // payload_merkle_root: [u8;32]
            // verifier_info: SigningVerifierSetInfo {
            //   signature: [u8;65], leaf: VerifierSetLeaf { nonce: u64, quorum: u128,
            //   signer_pubkey: [u8;33], signer_weight: u128, position: u16, set_size: u16,
            //   domain_separator: [u8;32] }, merkle_proof: Vec<u8>, payload_type: u8 }
            if args.len() >= 32 {
                println!(
                    "{indent}{} {}",
                    "payload_merkle_root:".dimmed(),
                    hex::encode(&args[..32])
                );
                let rest = &args[32..];
                // signature (65 bytes) + leaf fields
                if rest.len() >= 65 + 8 + 16 + 33 + 16 + 2 + 2 + 32 {
                    let sig = &rest[..65];
                    println!("{indent}{} 0x{}", "signature:".dimmed(), hex::encode(sig));
                    let leaf = &rest[65..];
                    let nonce = u64::from_le_bytes(leaf[..8].try_into().unwrap_or_default());
                    let quorum = u128::from_le_bytes(leaf[8..24].try_into().unwrap_or_default());
                    let signer_pubkey = hex::encode(&leaf[24..57]);
                    let signer_weight =
                        u128::from_le_bytes(leaf[57..73].try_into().unwrap_or_default());
                    let position = u16::from_le_bytes(leaf[73..75].try_into().unwrap_or_default());
                    let set_size = u16::from_le_bytes(leaf[75..77].try_into().unwrap_or_default());
                    println!("{indent}{} 0x{signer_pubkey}", "signer:".dimmed(),);
                    println!(
                        "{indent}{} {signer_weight} (quorum: {quorum})",
                        "weight:".dimmed(),
                    );
                    println!(
                        "{indent}{} {position}/{set_size} (nonce: {nonce})",
                        "position:".dimmed(),
                    );
                    let rest = &leaf[77 + 32..]; // skip domain_separator
                    if let Ok((proof, rest)) = decode_borsh_bytes(rest) {
                        println!("{indent}{} {} bytes", "merkle_proof:".dimmed(), proof.len());
                        if !rest.is_empty() {
                            let payload_type = match rest[0] {
                                0 => "ApproveMessages",
                                1 => "RotateSigners",
                                _ => "Unknown",
                            };
                            println!("{indent}{} {payload_type}", "payload_type:".dimmed());
                        }
                    }
                }
            }
        }
        "SendMemo" => {
            if let Ok((dest_chain, rest)) = decode_borsh_string(args) {
                println!("{indent}{} \"{dest_chain}\"", "destination_chain:".dimmed());
                if let Ok((dest_addr, rest)) = decode_borsh_string(rest) {
                    println!(
                        "{indent}{} \"{dest_addr}\"",
                        "destination_address:".dimmed()
                    );
                    if let Ok((memo, _)) = decode_borsh_string(rest) {
                        println!("{indent}{} \"{memo}\"", "memo:".dimmed());
                    }
                }
            }
        }
        "InterchainTransfer" => {
            if args.len() >= 32 {
                println!(
                    "{indent}{} {}",
                    "token_id:".dimmed(),
                    hex::encode(&args[..32])
                );
                let rest = &args[32..];
                if let Ok((dest_chain, rest)) = decode_borsh_string(rest) {
                    println!("{indent}{} \"{dest_chain}\"", "destination_chain:".dimmed());
                    if let Ok((dest_addr_bytes, rest)) = decode_borsh_bytes(rest) {
                        let dest_addr = format_address_bytes(&dest_addr_bytes);
                        println!("{indent}{} {dest_addr}", "destination_address:".dimmed());
                        if rest.len() >= 16 {
                            let amount =
                                u64::from_le_bytes(rest[..8].try_into().unwrap_or_default());
                            let gas_value =
                                u64::from_le_bytes(rest[8..16].try_into().unwrap_or_default());
                            println!("{indent}{} {amount}", "amount:".dimmed());
                            println!("{indent}{} {gas_value} lamports", "gas_value:".dimmed());
                        }
                    }
                }
            }
        }
        "Execute" => {
            // Message { cc_id: CrossChainId { chain: String, id: String },
            //   source_address: String, destination_chain: String,
            //   destination_address: String, payload_hash: [u8;32] }
            // payload: Vec<u8>
            if let Ok((chain, rest)) = decode_borsh_string(args)
                && let Ok((id, rest)) = decode_borsh_string(rest)
            {
                println!("{indent}{} {chain}-{id}", "cc_id:".dimmed());
                if let Ok((source_addr, rest)) = decode_borsh_string(rest) {
                    println!("{indent}{} \"{source_addr}\"", "source_address:".dimmed());
                    if let Ok((dest_chain, rest)) = decode_borsh_string(rest)
                        && let Ok((dest_addr, rest)) = decode_borsh_string(rest)
                    {
                        println!("{indent}{} \"{dest_chain}\"", "destination_chain:".dimmed());
                        println!(
                            "{indent}{} \"{dest_addr}\"",
                            "destination_address:".dimmed()
                        );
                        if rest.len() >= 32 {
                            println!(
                                "{indent}{} {}",
                                "payload_hash:".dimmed(),
                                hex::encode(&rest[..32])
                            );
                            let rest = &rest[32..];
                            if let Ok((payload_bytes, _)) = decode_borsh_bytes(rest) {
                                let (size, content) = decode_payload(&payload_bytes);
                                let payload_line = match content {
                                    Some(decoded) => format!("{size} → {decoded}"),
                                    None => size,
                                };
                                for (j, line) in payload_line.lines().enumerate() {
                                    if j == 0 {
                                        println!("{indent}{} {line}", "payload:".dimmed());
                                    } else {
                                        println!("{indent}{line}");
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        "ValidateMessage" | "ApproveMessage" => {
            // Both take a Message (or MerklizedMessage containing a Message) as first arg.
            // MerklizedMessage: MessageLeaf { Message, position, set_size, domain_sep } + proof
            // Message: CrossChainId { chain, id }, source_address, dest_chain, dest_addr, payload_hash
            if let Ok((chain, rest)) = decode_borsh_string(args)
                && let Ok((id, rest)) = decode_borsh_string(rest)
            {
                println!("{indent}{} {chain}-{id}", "cc_id:".dimmed());
                if let Ok((source_addr, rest)) = decode_borsh_string(rest)
                    && let Ok((dest_chain, rest)) = decode_borsh_string(rest)
                    && let Ok((dest_addr, rest)) = decode_borsh_string(rest)
                {
                    println!("{indent}{} \"{source_addr}\"", "source_address:".dimmed());
                    println!("{indent}{} \"{dest_chain}\"", "destination_chain:".dimmed());
                    println!(
                        "{indent}{} \"{dest_addr}\"",
                        "destination_address:".dimmed()
                    );
                    if rest.len() >= 32 {
                        println!(
                            "{indent}{} {}",
                            "payload_hash:".dimmed(),
                            hex::encode(&rest[..32])
                        );
                    }
                }
            }
        }
        "ExecuteDeployInterchainToken" => {
            // token_id: [u8;32], name: String, symbol: String, decimals: u8, minter: Vec<u8>
            if args.len() >= 32 {
                println!(
                    "{indent}{} {}",
                    "token_id:".dimmed(),
                    hex::encode(&args[..32])
                );
                let rest = &args[32..];
                if let Ok((name, rest)) = decode_borsh_string(rest)
                    && let Ok((symbol, rest)) = decode_borsh_string(rest)
                    && !rest.is_empty()
                {
                    println!("{indent}{} \"{name}\"", "name:".dimmed());
                    println!("{indent}{} \"{symbol}\"", "symbol:".dimmed());
                    println!("{indent}{} {}", "decimals:".dimmed(), rest[0]);
                }
            }
        }
        "ExecuteInterchainTransfer" => {
            // Anchor #[instruction] order: message: Message, source_chain: String,
            // source_address: Vec<u8>, destination_address: Pubkey,
            // token_id: [u8;32], amount: u64, data: Vec<u8>
            // Skip the Message struct, parse from source_chain onward
            if let Ok((_chain, rest)) = decode_borsh_string(args)
                && let Ok((_id, rest)) = decode_borsh_string(rest)
                && let Ok((_src_addr, rest)) = decode_borsh_string(rest)
                && let Ok((_dest_chain, rest)) = decode_borsh_string(rest)
                && let Ok((_dest_addr, rest)) = decode_borsh_string(rest)
                && rest.len() >= 32
            {
                // Past the Message, now: source_chain, source_address, dest, token_id, amount
                let rest = &rest[32..]; // skip payload_hash
                if let Ok((source_chain, rest)) = decode_borsh_string(rest) {
                    println!("{indent}{} \"{source_chain}\"", "source_chain:".dimmed());
                    if let Ok((source_addr, rest)) = decode_borsh_bytes(rest) {
                        println!(
                            "{indent}{} {}",
                            "source_address:".dimmed(),
                            format_address_bytes(&source_addr)
                        );
                        if rest.len() >= 72 {
                            let dest = Pubkey::try_from(&rest[..32]).ok();
                            let token_id = hex::encode(&rest[32..64]);
                            let amount =
                                u64::from_le_bytes(rest[64..72].try_into().unwrap_or_default());
                            println!("{indent}{} {token_id}", "token_id:".dimmed());
                            if let Some(dest) = dest {
                                println!("{indent}{} {dest}", "destination:".dimmed());
                            }
                            println!("{indent}{} {amount}", "amount:".dimmed());
                        }
                    }
                }
            }
        }
        _ => {
            if args.len() <= 64 {
                println!("{indent}{} {}", "data:".dimmed(), hex::encode(args));
            } else {
                println!("{indent}{} {} bytes", "data:".dimmed(), args.len());
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
