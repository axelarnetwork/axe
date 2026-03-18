use alloy::dyn_abi::{DynSolType, DynSolValue, JsonAbiExt};
use alloy::hex;
use alloy::json_abi::JsonAbi;
use eyre::{bail, Result};
use std::collections::HashMap;
use std::sync::LazyLock;

const ABI_JSON: &str = include_str!("../../abi-db.json");

static FUNC_DB: LazyLock<HashMap<[u8; 4], alloy::json_abi::Function>> = LazyLock::new(|| {
    let abi: JsonAbi = serde_json::from_str(ABI_JSON).expect("embedded ABI is invalid");
    let mut map = HashMap::new();
    for funcs in abi.functions.values() {
        for func in funcs {
            map.insert(func.selector().0, func.clone());
        }
    }
    map
});

const MAX_DEPTH: usize = 5;

pub fn run(calldata_hex: &str) -> Result<()> {
    let data = parse_hex(calldata_hex)?;

    // Try as function call (4-byte selector lookup)
    if data.len() >= 4
        && let Some(func) = FUNC_DB.get(&<[u8; 4]>::try_from(&data[..4])?) {
            let values = func.abi_decode_input(&data[4..])?;
            println!("{}", func.signature());
            print_decoded(&func.inputs, &values, "  ", 0);
            return Ok(());
        }

    // Try as ITS payload
    if try_print_its(&data, "", 0) {
        return Ok(());
    }

    // Try fallback patterns
    if try_print_fallback(&data, "") {
        return Ok(());
    }

    if data.len() >= 4 {
        bail!(
            "unknown selector 0x{}, not recognized",
            hex::encode(&data[..4])
        );
    }
    bail!("could not decode data, not recognized")
}

fn parse_hex(s: &str) -> Result<Vec<u8>> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let clean = clean.strip_prefix("0x").unwrap_or(&clean);
    hex::decode(clean).map_err(|e| eyre::eyre!("invalid hex: {e}"))
}

fn format_value(v: &DynSolValue) -> String {
    match v {
        DynSolValue::Bool(b) => b.to_string(),
        DynSolValue::Int(n, _) => n.to_string(),
        DynSolValue::Uint(n, _) => n.to_string(),
        DynSolValue::FixedBytes(word, size) => format!("0x{}", hex::encode(&word[..*size])),
        DynSolValue::Address(a) => format!("{a}"),
        DynSolValue::Bytes(b) => {
            let h = hex::encode(b);
            if h.len() > 128 {
                format!("0x{}… ({} bytes)", &h[..64], b.len())
            } else {
                format!("0x{h}")
            }
        }
        DynSolValue::String(s) => format!("\"{s}\""),
        DynSolValue::Array(arr) | DynSolValue::FixedArray(arr) => {
            format!(
                "[{}]",
                arr.iter().map(format_value).collect::<Vec<_>>().join(", ")
            )
        }
        DynSolValue::Tuple(items) => {
            format!(
                "({})",
                items
                    .iter()
                    .map(format_value)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        _ => format!("{v:?}"),
    }
}

fn value_as_u64(v: &DynSolValue) -> Option<u64> {
    if let DynSolValue::Uint(n, _) = v {
        let limbs = n.as_limbs();
        if limbs[1] == 0 && limbs[2] == 0 && limbs[3] == 0 {
            return Some(limbs[0]);
        }
    }
    None
}

fn enum_label(field: &str, value: &DynSolValue) -> String {
    let Some(v) = value_as_u64(value) else {
        return String::new();
    };
    match field {
        "messageType" => its_message_type_name(v).map(|s| format!(" ({s})")),
        "tokenManagerType" => token_manager_type_name(v).map(|s| format!(" ({s})")),
        _ => None,
    }
    .unwrap_or_default()
}

fn print_decoded(
    inputs: &[alloy::json_abi::Param],
    values: &[DynSolValue],
    indent: &str,
    depth: usize,
) {
    for (i, (param, value)) in inputs.iter().zip(values.iter()).enumerate() {
        let name = if param.name.is_empty() {
            format!("arg{i}")
        } else {
            param.name.clone()
        };
        println!(
            "{indent}{name} ({}): {}{}",
            param.ty,
            format_value(value),
            enum_label(&name, value)
        );

        if depth < MAX_DEPTH {
            match (&*param.ty, value) {
                ("bytes", DynSolValue::Bytes(b)) if b.len() >= 4 => {
                    try_print_nested(b, &format!("{indent}  "), depth + 1);
                }
                ("bytes[]", DynSolValue::Array(arr)) => {
                    let nested = format!("{indent}  ");
                    for (j, item) in arr.iter().enumerate() {
                        if let DynSolValue::Bytes(b) = item
                            && b.len() >= 4 {
                                println!(
                                    "\n{indent}[{j}]: 0x{} ",
                                    hex::encode(&b[..4])
                                );
                                try_print_nested(b, &format!("{nested}  "), depth + 1);
                            }
                    }
                }
                _ => {}
            }
        }
    }
}

fn try_print_nested(data: &[u8], indent: &str, depth: usize) -> bool {
    if depth > MAX_DEPTH || data.is_empty() {
        return false;
    }

    // Try as function call
    if data.len() >= 4
        && let Ok(sel) = <[u8; 4]>::try_from(&data[..4])
            && let Some(func) = FUNC_DB.get(&sel)
                && let Ok(values) = func.abi_decode_input(&data[4..]) {
                    println!("{indent}{}", func.signature());
                    print_decoded(&func.inputs, &values, &format!("{indent}  "), depth);
                    return true;
                }

    // Try ITS payload
    if try_print_its(data, indent, depth) {
        return true;
    }

    // Try as printable UTF-8
    if let Ok(s) = std::str::from_utf8(data)
        && !s.is_empty() && s.chars().all(|c| c.is_ascii_graphic() || c.is_ascii_whitespace()) {
            println!("{indent}→ \"{s}\"");
            return true;
        }

    false
}

fn try_print_its(data: &[u8], indent: &str, depth: usize) -> bool {
    if data.len() < 32 || depth > MAX_DEPTH {
        return false;
    }

    // Message type is first uint256 — upper 24 bytes must be zero
    if data[..24].iter().any(|&b| b != 0) {
        return false;
    }

    let Ok(bytes) = <[u8; 8]>::try_from(&data[24..32]) else {
        return false;
    };
    let msg_type = u64::from_be_bytes(bytes);

    let Some((name, types, labels)) = its_message_def(msg_type) else {
        return false;
    };

    let tuple_type = DynSolType::Tuple(types.clone());
    let Ok(DynSolValue::Tuple(values)) = tuple_type.abi_decode_params(data) else {
        return false;
    };

    println!("{indent}[ITS {name}]");
    for (value, (label, ty)) in values.iter().zip(labels.iter().zip(types.iter())) {
        println!(
            "{indent}  {label} ({ty}): {}{}",
            format_value(value),
            enum_label(label, value)
        );

        if depth < MAX_DEPTH
            && let DynSolValue::Bytes(b) = value
                && b.len() >= 4 {
                    try_print_nested(b, &format!("{indent}    "), depth + 1);
                }
    }
    true
}

fn try_print_fallback(data: &[u8], indent: &str) -> bool {
    let patterns: &[&[DynSolType]] = &[
        &[DynSolType::String, DynSolType::Bytes, DynSolType::Bytes],
        &[DynSolType::String, DynSolType::String, DynSolType::Bytes],
        &[DynSolType::String],
        &[DynSolType::Bytes],
        &[
            DynSolType::Address,
            DynSolType::Uint(256),
            DynSolType::Bytes,
        ],
        &[DynSolType::FixedBytes(32), DynSolType::Bytes],
    ];

    for pattern in patterns {
        let tuple = DynSolType::Tuple(pattern.to_vec());
        if let Ok(DynSolValue::Tuple(values)) = tuple.abi_decode_params(data) {
            let type_str = pattern
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            println!("{indent}Decoded as ({type_str}):");
            for (i, (value, ty)) in values.iter().zip(pattern.iter()).enumerate() {
                println!("{indent}  arg{i} ({ty}): {}", format_value(value));
                if let (DynSolType::Bytes, DynSolValue::Bytes(b)) = (ty, value)
                    && b.len() >= 4
                {
                    try_print_nested(b, &format!("{indent}    "), 1);
                }
            }
            return true;
        }
    }
    false
}

fn its_message_def(msg_type: u64) -> Option<(&'static str, Vec<DynSolType>, Vec<&'static str>)> {
    Some(match msg_type {
        0 => (
            "INTERCHAIN_TRANSFER",
            vec![
                DynSolType::Uint(256),
                DynSolType::FixedBytes(32),
                DynSolType::Bytes,
                DynSolType::Bytes,
                DynSolType::Uint(256),
                DynSolType::Bytes,
            ],
            vec![
                "messageType",
                "tokenId",
                "sourceAddress",
                "destinationAddress",
                "amount",
                "data",
            ],
        ),
        1 => (
            "DEPLOY_INTERCHAIN_TOKEN",
            vec![
                DynSolType::Uint(256),
                DynSolType::FixedBytes(32),
                DynSolType::String,
                DynSolType::String,
                DynSolType::Uint(8),
                DynSolType::Bytes,
            ],
            vec![
                "messageType",
                "tokenId",
                "name",
                "symbol",
                "decimals",
                "minter",
            ],
        ),
        3 => (
            "SEND_TO_HUB",
            vec![
                DynSolType::Uint(256),
                DynSolType::String,
                DynSolType::Bytes,
            ],
            vec!["messageType", "destinationChain", "payload"],
        ),
        4 => (
            "RECEIVE_FROM_HUB",
            vec![
                DynSolType::Uint(256),
                DynSolType::String,
                DynSolType::Bytes,
            ],
            vec!["messageType", "sourceChain", "payload"],
        ),
        5 => (
            "LINK_TOKEN",
            vec![
                DynSolType::Uint(256),
                DynSolType::FixedBytes(32),
                DynSolType::Uint(8),
                DynSolType::Bytes,
                DynSolType::Bytes,
                DynSolType::Bytes,
            ],
            vec![
                "messageType",
                "tokenId",
                "tokenManagerType",
                "sourceTokenAddress",
                "destinationTokenAddress",
                "linkParams",
            ],
        ),
        6 => (
            "REGISTER_TOKEN_METADATA",
            vec![
                DynSolType::Uint(256),
                DynSolType::Bytes,
                DynSolType::Uint(8),
            ],
            vec!["messageType", "tokenAddress", "decimals"],
        ),
        _ => return None,
    })
}

fn its_message_type_name(v: u64) -> Option<&'static str> {
    match v {
        0 => Some("INTERCHAIN_TRANSFER"),
        1 => Some("DEPLOY_INTERCHAIN_TOKEN"),
        3 => Some("SEND_TO_HUB"),
        4 => Some("RECEIVE_FROM_HUB"),
        5 => Some("LINK_TOKEN"),
        6 => Some("REGISTER_TOKEN_METADATA"),
        _ => None,
    }
}

fn token_manager_type_name(v: u64) -> Option<&'static str> {
    match v {
        0 => Some("NATIVE_INTERCHAIN_TOKEN"),
        1 => Some("MINT_BURN_FROM"),
        2 => Some("LOCK_UNLOCK"),
        3 => Some("LOCK_UNLOCK_FEE"),
        4 => Some("MINT_BURN"),
        5 => Some("GATEWAY"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_register_custom_token() {
        run(
            "0xd8c032689e52713efc11c03e5a032d47a49317e5322bd17fe623afee2cbf25603e3fb340000000000000000000000000fc450df8c19670b6a7f18092fc4aed43e9b8bf5600000000000000000000000000000000000000000000000000000000000000010000000000000000000000005ae7ec463b0b97635fc0e57a0129a386a34cccb5",
        ).unwrap();
    }

    #[test]
    fn decode_link_token() {
        run(
            "0x0f4433d39e52713efc11c03e5a032d47a49317e5322bd17fe623afee2cbf25603e3fb34000000000000000000000000000000000000000000000000000000000000000c0000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000140000000000000000000000000000000000000000000000000002386f26fc100000000000000000000000000000000000000000000000000000000000000000010657468657265756d2d7365706f6c6961000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000014450facbddc1a261bd9e29ccb476ff370a3f448fc00000000000000000000000000000000000000000000000000000000000000000000000000000000000000145ae7ec463b0b97635fc0e57a0129a386a34cccb5000000000000000000000000",
        ).unwrap();
    }

    #[test]
    fn decode_execute_with_payload() {
        run(
            "0x49160658f739a5a827c0d9a97fa5b40c56c638785355777fbb135b8ffb54457b10094b65000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000001200000000000000000000000000000000000000000000000000000000000000009736f6c616e612d31380000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002c4356384c626b36595457376a7453686838634c7939346b6d574665357545686847555a6e335a78686255654c000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000003968656c6c6f2066726f6d20617865206c6f6164207465737420393465336130343363363833313836323130663132383631633563376664646600000000000000",
        ).unwrap();
    }

    #[test]
    fn decode_deploy_interchain_token() {
        run(
            "0x3e12f8c533b7f5b5f2a8c055cd44c8671d2ba3c10f1e00506232d8f4b1095ef9988257fc00000000000000000000000000000000000000000000000000000000000000c00000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000001200000000000000000000000000000000000000000000003635c9adc5dea0000000000000000000000000000081e63ea8f64fedb9858eb6e2176b431fbd10d1ec00000000000000000000000000000000000000000000000000000000000000074d79546f6b656e0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000034d544b0000000000000000000000000000000000000000000000000000000000",
        ).unwrap();
    }

    #[test]
    fn decode_handles_whitespace_in_hex() {
        // Same as deploy_interchain_token but with spaces
        run(
            "0x3e12f8c5 33b7f5b5f2a8c055cd44c8671d2ba3c10f1e00506232d8f4b1095ef9988257fc 00000000000000000000000000000000000000000000000000000000000000c0 0000000000000000000000000000000000000000000000000000000000000100 0000000000000000000000000000000000000000000000000000000000000012 00000000000000000000000000000000000000000000003635c9adc5dea00000 00000000000000000000000081e63ea8f64fedb9858eb6e2176b431fbd10d1ec 0000000000000000000000000000000000000000000000000000000000000007 4d79546f6b656e00000000000000000000000000000000000000000000000000 0000000000000000000000000000000000000000000000000000000000000003 4d544b0000000000000000000000000000000000000000000000000000000000",
        ).unwrap();
    }

    #[test]
    fn decode_unknown_selector_errors() {
        let result = run("0xdeadbeef00000000000000000000000000000000000000000000000000000000");
        assert!(result.is_err());
    }
}
