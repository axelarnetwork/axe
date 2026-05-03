//! Typed wrapper around the `axelar-contract-deployments` chains-config JSON.
//!
//! The on-disk schema (testnet.json, mainnet.json, devnet-amplifier.json,
//! stagenet.json) is sprawling and grows over time; we only type the fields
//! we actually read. Anything we don't enumerate is absorbed by the
//! `extra: HashMap<String, Value>` flattening on each struct, so adding new
//! fields to the deployments repo does not break parsing here.
//!
//! Every field is `Option<T>` because the deployments repo is the source of
//! truth and we don't want a missing entry on one chain to fail-stop the
//! whole load. Callers that *require* a field should `?` it explicitly with
//! a clear error message instead of silently defaulting.

// Many fields are typed for completeness but only become live once the
// Phase 1b/1c readers and command call-sites are migrated; allow dead code
// at module scope for now and revisit once migration is complete.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;

use eyre::{Result, WrapErr};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct ChainsConfig {
    pub chains: HashMap<String, ChainConfig>,
    pub axelar: AxelarConfig,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

impl ChainsConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("failed to read chains config '{}'", path.display()))?;
        Self::from_json_str(&s)
            .wrap_err_with(|| format!("failed to parse chains config '{}'", path.display()))
    }

    pub fn from_json_str(s: &str) -> Result<Self> {
        Ok(serde_json::from_str(s)?)
    }
}

/// One entry under the top-level `chains` map (e.g. `chains.flow`,
/// `chains.solana`). `chain_type` stays a raw `String` because the on-disk
/// set ("evm", "svm", "stellar", "sui", "xrpl") is wider than the
/// `crate::types::ChainType` enum's closed set; callers that only handle
/// evm/svm should `.parse::<ChainType>()` and propagate the error.
#[derive(Debug, Deserialize)]
pub struct ChainConfig {
    #[serde(rename = "axelarId")]
    pub axelar_id: Option<String>,
    pub name: Option<String>,
    pub rpc: Option<String>,
    #[serde(rename = "chainType")]
    pub chain_type: Option<String>,
    #[serde(rename = "tokenSymbol")]
    pub token_symbol: Option<String>,
    pub decimals: Option<u8>,
    pub contracts: Option<HashMap<String, ContractEntry>>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Top-level `axelar` block. Holds Axelar-network connection fields plus the
/// nested `contracts` map keyed by contract name then by chain id (e.g.
/// `axelar.contracts.Gateway.flow.address`). Some inner maps mix chain
/// entries with metadata keys like `lastUploadedCodeId`, so the inner value
/// stays untyped (`HashMap<String, Value>`); see [`AxelarConfig::contract_address`].
#[derive(Debug, Deserialize)]
pub struct AxelarConfig {
    #[serde(rename = "axelarId")]
    pub axelar_id: Option<String>,
    #[serde(rename = "chainId")]
    pub chain_id: Option<String>,
    pub lcd: Option<String>,
    pub rpc: Option<String>,
    pub grpc: Option<String>,
    /// Raw gas-price string with denom suffix, e.g. `"0.007uaxl"`. Use
    /// [`AxelarConfig::parse_gas_price`] to split into the numeric price and
    /// denom.
    #[serde(rename = "gasPrice")]
    pub gas_price: Option<String>,
    #[serde(rename = "tokenSymbol")]
    pub token_symbol: Option<String>,
    #[serde(rename = "unitDenom")]
    pub unit_denom: Option<String>,
    #[serde(rename = "governanceAddress")]
    pub governance_address: Option<String>,
    #[serde(rename = "adminAddress")]
    pub admin_address: Option<String>,
    #[serde(rename = "multisigProverAdminAddress")]
    pub multisig_prover_admin_address: Option<String>,
    /// Stringified base-denom amount (e.g. `"2000000000"`) used as the
    /// deposit when submitting governance proposals on the Axelar hub.
    #[serde(rename = "govProposalDepositAmount")]
    pub gov_proposal_deposit_amount: Option<String>,
    /// Stringified base-denom amount used for expedited governance proposals.
    #[serde(rename = "govProposalExpeditedDepositAmount")]
    pub gov_proposal_expedited_deposit_amount: Option<String>,
    pub contracts: Option<HashMap<String, HashMap<String, Value>>>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

impl AxelarConfig {
    /// Look up `axelar.contracts.<contract>.<chain>.address`. Errors with a
    /// `field not found` message if the contract entry, chain entry, or
    /// address string is missing — callers can `.ok()` to opt back into
    /// `Option` semantics where absence is acceptable.
    pub fn contract_address(&self, contract: &str, chain: &str) -> Result<&str> {
        let opt = (|| -> Option<&str> {
            self.contracts
                .as_ref()?
                .get(contract)?
                .get(chain)?
                .get("address")?
                .as_str()
        })();
        opt.ok_or_else(|| {
            eyre::eyre!("no axelar.contracts.{contract}.{chain}.address in target json")
        })
    }

    /// Look up `axelar.contracts.<contract>.address` — the chain-agnostic
    /// (global) form used by hub-level contracts like AxelarnetGateway,
    /// Router, Multisig, Coordinator, etc., which don't have a per-chain
    /// breakdown at this level.
    pub fn global_contract_address(&self, contract: &str) -> Result<&str> {
        let opt = (|| -> Option<&str> {
            self.contracts
                .as_ref()?
                .get(contract)?
                .get("address")?
                .as_str()
        })();
        opt.ok_or_else(|| eyre::eyre!("no axelar.contracts.{contract}.address in target json"))
    }

    /// Parse the raw `"0.007uaxl"`-shaped `gasPrice` into `(price, denom)`.
    /// Errors if the field is missing or doesn't carry a numeric prefix;
    /// silent defaults are unsafe because the hub binds the denom and price
    /// to specific deployments.
    pub fn parse_gas_price(&self) -> Result<(f64, String)> {
        let raw = self
            .gas_price
            .as_deref()
            .ok_or_else(|| eyre::eyre!("no axelar.gasPrice in target json"))?;
        let split = raw
            .find(|c: char| c.is_alphabetic())
            .ok_or_else(|| eyre::eyre!("axelar.gasPrice '{raw}' has no denom suffix"))?;
        let price: f64 = raw[..split]
            .parse()
            .map_err(|e| eyre::eyre!("axelar.gasPrice '{raw}' numeric prefix invalid: {e}"))?;
        Ok((price, raw[split..].to_string()))
    }

    /// The four fields needed to sign and broadcast a cosmos tx against the
    /// Axelar hub: `(lcd, chain_id, fee_denom, gas_price)`. Errors if any
    /// of them are missing or unparseable. Replaces the old free-standing
    /// `cosmos::read_axelar_config` once all callers have switched to the
    /// typed config.
    pub fn cosmos_tx_params(&self) -> Result<(String, String, String, f64)> {
        let (price, denom) = self.parse_gas_price()?;
        let lcd = self
            .lcd
            .clone()
            .ok_or_else(|| eyre::eyre!("no axelar.lcd in target json"))?;
        let chain_id = self
            .chain_id
            .clone()
            .ok_or_else(|| eyre::eyre!("no axelar.chainId in target json"))?;
        Ok((lcd, chain_id, denom, price))
    }
}

/// One entry under a chain's `contracts` map (e.g.
/// `chains.flow.contracts.AxelarGateway`). Most callers only read `address`;
/// the rest of the deployer-side fields are kept as `extra` so we don't
/// need to model the full `axelar-contract-deployments` shape here.
#[derive(Debug, Deserialize)]
pub struct ContractEntry {
    pub address: Option<String>,
    pub implementation: Option<String>,
    pub deployer: Option<String>,
    pub salt: Option<String>,
    pub version: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

impl ChainConfig {
    /// Look up `chains.<this>.contracts.<contract>.address`. Errors with a
    /// `not deployed yet` message if the contract entry or address is
    /// missing — callers can `.ok()` to opt back into `Option` semantics.
    pub fn contract_address(&self, contract: &str, axelar_id: &str) -> Result<&str> {
        let opt =
            (|| -> Option<&str> { self.contracts.as_ref()?.get(contract)?.address.as_deref() })();
        opt.ok_or_else(|| eyre::eyre!("{contract} not deployed yet for {axelar_id}"))
    }

    /// Return this chain's `axelarId`, falling back to the supplied JSON key
    /// when the field is absent. The cosmos-side `axelarId` often differs
    /// from the JSON key (e.g. JSON key `"avalanche"` but
    /// `axelarId: "Avalanche"`); for chains where it isn't set, callers
    /// conventionally reuse the JSON key.
    pub fn axelar_id_or(&self, fallback: &str) -> String {
        self.axelar_id
            .clone()
            .unwrap_or_else(|| fallback.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal fixture covering every field the loader currently types.
    /// Mirrors a sliver of testnet.json without depending on the external
    /// repo at test time.
    const FIXTURE: &str = r#"
    {
      "chains": {
        "flow": {
          "axelarId": "flow",
          "name": "Flow",
          "rpc": "https://testnet.evm.nodes.onflow.org",
          "chainType": "evm",
          "tokenSymbol": "FLOW",
          "decimals": 18,
          "contracts": {
            "AxelarGateway": {
              "address": "0xe432150cce91c13a887f7D836923d5597adD8E31",
              "salt": "v6.0.4"
            }
          },
          "explorer": { "url": "https://evm-testnet.flowscan.io" }
        },
        "solana": {
          "axelarId": "solana",
          "chainType": "svm",
          "rpc": "https://api.devnet.solana.com",
          "tokenSymbol": "SOL",
          "decimals": 9
        }
      },
      "axelar": {
        "axelarId": "axelarnet",
        "chainId": "axelar-testnet-lisbon-3",
        "lcd": "https://lcd-axelar-testnet.imperator.co",
        "gasPrice": "0.007uaxl",
        "tokenSymbol": "AXL",
        "unitDenom": "uaxl",
        "contracts": {
          "Gateway": {
            "flow": {
              "address": "axelar1w8frw33jn0yx59845wdgk0yru6fxvgr6hlh4xfdtdf08y5jamcnsyu0z6u",
              "version": "1.1.1"
            },
            "lastUploadedCodeId": 24
          }
        }
      },
      "someUnknownTopLevelKey": { "ignored": true }
    }
    "#;

    #[test]
    fn parses_typed_fields() {
        let cfg = ChainsConfig::from_json_str(FIXTURE).expect("fixture parses");

        let flow = cfg.chains.get("flow").expect("flow present");
        assert_eq!(flow.axelar_id.as_deref(), Some("flow"));
        assert_eq!(flow.chain_type.as_deref(), Some("evm"));
        assert_eq!(flow.token_symbol.as_deref(), Some("FLOW"));
        assert_eq!(flow.decimals, Some(18));
        assert_eq!(
            flow.contract_address("AxelarGateway", "flow").unwrap(),
            "0xe432150cce91c13a887f7D836923d5597adD8E31",
        );
        assert!(flow.contract_address("AxelarMissing", "flow").is_err());

        let sol = cfg.chains.get("solana").expect("solana present");
        assert_eq!(sol.chain_type.as_deref(), Some("svm"));
        assert!(sol.contracts.is_none());
    }

    #[test]
    fn parses_axelar_block_and_helpers() {
        let cfg = ChainsConfig::from_json_str(FIXTURE).expect("fixture parses");

        assert_eq!(
            cfg.axelar.lcd.as_deref(),
            Some("https://lcd-axelar-testnet.imperator.co")
        );
        assert_eq!(
            cfg.axelar.chain_id.as_deref(),
            Some("axelar-testnet-lisbon-3")
        );

        let (price, denom) = cfg.axelar.parse_gas_price().expect("gas price parses");
        assert!((price - 0.007).abs() < 1e-12);
        assert_eq!(denom, "uaxl");

        let (lcd, chain_id, fee_denom, gas_price) =
            cfg.axelar.cosmos_tx_params().expect("tx params resolve");
        assert_eq!(lcd, "https://lcd-axelar-testnet.imperator.co");
        assert_eq!(chain_id, "axelar-testnet-lisbon-3");
        assert_eq!(fee_denom, "uaxl");
        assert!((gas_price - 0.007).abs() < 1e-12);

        assert_eq!(
            cfg.axelar.contract_address("Gateway", "flow").unwrap(),
            "axelar1w8frw33jn0yx59845wdgk0yru6fxvgr6hlh4xfdtdf08y5jamcnsyu0z6u",
        );
        // Metadata keys (lastUploadedCodeId) carry no `address` field, so the
        // lookup surfaces as a `field not found` error rather than panicking.
        assert!(
            cfg.axelar
                .contract_address("Gateway", "lastUploadedCodeId")
                .is_err()
        );
    }

    #[test]
    fn unknown_top_level_keys_absorbed() {
        let cfg = ChainsConfig::from_json_str(FIXTURE).expect("fixture parses");
        assert!(cfg.extra.contains_key("someUnknownTopLevelKey"));
    }

    /// Sanity check against a real on-disk config. Ignored by default
    /// because it depends on the sibling `axelar-contract-deployments` repo
    /// being checked out at the canonical relative path. Run manually after
    /// schema bumps with `cargo test -- --ignored loads_real_testnet_json`.
    #[test]
    #[ignore = "depends on sibling axelar-contract-deployments checkout"]
    fn loads_real_testnet_json() {
        let path =
            Path::new("../axelar-contract-deployments/axelar-chains-config/info/testnet.json");
        let cfg = ChainsConfig::load(path).expect("testnet.json loads + parses");
        assert!(cfg.chains.contains_key("flow"), "flow chain present");
        assert!(cfg.chains.contains_key("solana"), "solana chain present");
        assert!(cfg.axelar.lcd.is_some(), "axelar.lcd present");
        assert!(
            cfg.axelar.parse_gas_price().is_ok(),
            "axelar.gasPrice parses",
        );
    }
}
