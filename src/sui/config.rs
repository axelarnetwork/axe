//! Sui chains-config readers: pull RPC URL, the Example/AxelarGateway/
//! GasService object IDs, and the AxelarGateway Move-package address out of
//! the JSON config the load test ships with.

use eyre::{Result, eyre};
use serde_json::Value;
use sui_sdk_types::Address as SuiAddress;

#[derive(Debug, Clone)]
pub struct SuiContractsConfig {
    pub example_pkg: SuiAddress,
    pub gmp_singleton: SuiAddress,
    pub gateway_object: SuiAddress,
    pub gas_service_object: SuiAddress,
}

/// Read Sui chain config (RPC + key contract object IDs) from the chains config JSON.
pub fn read_sui_chain_config(
    config: &std::path::Path,
    chain_id: &str,
) -> Result<(String, SuiContractsConfig)> {
    let content =
        std::fs::read_to_string(config).map_err(|e| eyre!("failed to read config: {e}"))?;
    let root: Value = serde_json::from_str(&content)?;
    let chain = root
        .pointer(&format!("/chains/{chain_id}"))
        .ok_or_else(|| eyre!("chain '{chain_id}' not found in config"))?;
    let rpc = chain
        .get("rpc")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("no rpc for sui chain '{chain_id}'"))?
        .to_string();

    let example_pkg = chain
        .pointer("/contracts/Example/address")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("no Example.address for '{chain_id}'"))?;
    let gmp_singleton = chain
        .pointer("/contracts/Example/objects/GmpSingleton")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("no Example.objects.GmpSingleton for '{chain_id}'"))?;
    let gateway_object = chain
        .pointer("/contracts/AxelarGateway/objects/Gateway")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("no AxelarGateway.objects.Gateway for '{chain_id}'"))?;
    let gas_service_object = chain
        .pointer("/contracts/GasService/objects/GasService")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("no GasService.objects.GasService for '{chain_id}'"))?;

    Ok((
        rpc,
        SuiContractsConfig {
            example_pkg: parse_sui_addr(example_pkg)?,
            gmp_singleton: parse_sui_addr(gmp_singleton)?,
            gateway_object: parse_sui_addr(gateway_object)?,
            gas_service_object: parse_sui_addr(gas_service_object)?,
        },
    ))
}

pub fn parse_sui_addr(s: &str) -> Result<SuiAddress> {
    SuiAddress::from_hex(s).map_err(|e| eyre!("Sui address parse '{s}': {e:?}"))
}

/// Read the AxelarGateway Move-package address for a Sui chain. Used by the
/// destination-side verifier to construct event-type strings for
/// `events::MessageApproved` / `events::MessageExecuted`.
pub fn read_sui_gateway_pkg(config: &std::path::Path, chain_id: &str) -> Result<String> {
    let content =
        std::fs::read_to_string(config).map_err(|e| eyre!("failed to read config: {e}"))?;
    let root: Value = serde_json::from_str(&content)?;
    let pkg = root
        .pointer(&format!(
            "/chains/{chain_id}/contracts/AxelarGateway/address"
        ))
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("no AxelarGateway.address for sui chain '{chain_id}'"))?;
    Ok(pkg.to_string())
}
