//! Config + cache resolution for the load-test command:
//! `LoadTestArgs` → `ResolvedConfig`, JSON cache file IO, the auto-detect
//! heuristics that pick a `(source, destination)` pair when the user only
//! supplies `--config`, and the cargo-feature-driven network sanity check.

use std::collections::HashMap;
use std::path::PathBuf;

use eyre::{Result, WrapErr};
use serde::Deserialize;
use serde_json::json;

use super::TestType;
use crate::ui;

/// Subset of the chains-config JSON read by the resolver. The map is keyed by
/// the JSON-side chain id (e.g. `"avalanche"`); each entry's `axelarId` may
/// differ from that key (e.g. `"Avalanche"` for consensus chains).
#[derive(Deserialize)]
struct ConfigRoot {
    chains: HashMap<String, ChainEntry>,
}

#[derive(Deserialize)]
struct ChainEntry {
    #[serde(rename = "axelarId")]
    axelar_id: Option<String>,
    #[serde(rename = "chainType")]
    chain_type: Option<String>,
    rpc: Option<String>,
}

pub(crate) fn cache_path(axelar_id: &str) -> PathBuf {
    let data_dir = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("axe");
    data_dir.join(format!("load-test-{axelar_id}.json"))
}

pub(crate) fn read_cache(axelar_id: &str) -> serde_json::Value {
    let path = cache_path(axelar_id);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

pub(crate) fn save_cache(axelar_id: &str, cache: &serde_json::Value) -> Result<()> {
    let path = cache_path(axelar_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(cache)?)?;
    Ok(())
}
fn chain_type(chains: &HashMap<String, ChainEntry>, chain_id: &str) -> Option<String> {
    chains.get(chain_id)?.chain_type.clone()
}

/// Find chains by chainType, optionally skipping core-* prefixed chains.
fn find_chains_by_type(
    chains: &HashMap<String, ChainEntry>,
    chain_type_filter: &str,
    skip_core: bool,
) -> Vec<String> {
    chains
        .iter()
        .filter(|(k, v)| {
            v.chain_type.as_deref() == Some(chain_type_filter)
                && !(skip_core && k.starts_with("core-"))
        })
        .map(|(k, _)| k.clone())
        .collect()
}

/// Infer test type from source and destination chain types.
pub(crate) fn infer_test_type(source_type: &str, dest_type: &str) -> Result<TestType> {
    match (source_type, dest_type) {
        ("svm", "evm") => Ok(TestType::SolToEvm),
        ("evm", "svm") => Ok(TestType::EvmToSol),
        ("evm", "evm") => Ok(TestType::EvmToEvm),
        ("svm", "svm") => Ok(TestType::SolToSol),
        ("xrpl", "evm") => Ok(TestType::XrplToEvm),
        ("evm", "xrpl") => Ok(TestType::EvmToXrpl),
        ("stellar", "evm") => Ok(TestType::StellarToEvm),
        ("evm", "stellar") => Ok(TestType::EvmToStellar),
        ("stellar", "svm") => Ok(TestType::StellarToSol),
        ("svm", "stellar") => Ok(TestType::SolToStellar),
        ("sui", "evm") => Ok(TestType::SuiToEvm),
        ("evm", "sui") => Ok(TestType::EvmToSui),
        ("sui", "svm") => Ok(TestType::SuiToSol),
        ("svm", "sui") => Ok(TestType::SolToSui),
        ("sui", "stellar") => Ok(TestType::SuiToStellar),
        ("stellar", "sui") => Ok(TestType::StellarToSui),
        ("sui", "xrpl") => Ok(TestType::SuiToXrpl),
        ("xrpl", "sui") => Ok(TestType::XrplToSui),
        _ => Err(eyre::eyre!(
            "unsupported chain type combination: {source_type} -> {dest_type}. \
             Supported: svm -> evm, evm -> svm, evm -> evm, svm -> svm, \
             xrpl -> evm, evm -> xrpl, stellar -> evm, evm -> stellar, \
             stellar -> svm, svm -> stellar, sui <-> {{evm, svm, stellar, xrpl}}"
        )),
    }
}
pub struct ResolvedConfig {
    pub test_type: TestType,
    pub source_chain: String,
    pub destination_chain: String,
    /// The `axelarId` for the source chain — may differ from the JSON key
    /// (e.g. `"Avalanche"` vs `"avalanche"` for consensus chains).
    pub source_axelar_id: String,
    /// The `axelarId` for the destination chain.
    pub destination_axelar_id: String,
    pub source_rpc: String,
    pub destination_rpc: String,
    pub private_key: Option<String>,
}

/// Resolve chains, RPCs, and test type from the config JSON.
///
/// Supports three modes:
pub(crate) fn resolve_from_config(
    config: &PathBuf,
    test_type_override: Option<TestType>,
    source_chain_override: Option<String>,
    destination_chain_override: Option<String>,
    private_key_override: Option<String>,
    source_rpc_override: Option<String>,
    destination_rpc_override: Option<String>,
) -> Result<ResolvedConfig> {
    let config_content = std::fs::read_to_string(config)
        .map_err(|e| eyre::eyre!("failed to read config {}: {e}", config.display()))?;
    let config_root: ConfigRoot = serde_json::from_str(&config_content)
        .with_context(|| format!("no 'chains' object in config {}", config.display()))?;
    let chains = &config_root.chains;

    // --- Resolve test type + chains ---
    let (test_type, source_chain, destination_chain) = match (
        test_type_override,
        source_chain_override,
        destination_chain_override,
    ) {
        // Case 1: Both chains given → infer test type
        (None, Some(src), Some(dst)) => {
            let src_type = chain_type(chains, &src)
                .ok_or_else(|| eyre::eyre!("source chain '{src}' not found in config"))?;
            let dst_type = chain_type(chains, &dst)
                .ok_or_else(|| eyre::eyre!("destination chain '{dst}' not found in config"))?;
            let tt = infer_test_type(&src_type, &dst_type)?;
            ui::info(&format!("inferred test type: {tt}"));
            (tt, src, dst)
        }
        // Case 2: Test type + optional overrides → auto-detect missing chains
        (Some(tt), src_opt, dst_opt) => {
            let (src, dst) = auto_detect_chains(chains, tt, src_opt, dst_opt)?;
            (tt, src, dst)
        }
        // Case 3: Nothing or partial → try to auto-detect everything
        (None, src_opt, dst_opt) => {
            // Try to find a valid combination from config
            let (tt, src, dst) = auto_detect_all(chains, src_opt, dst_opt)?;
            (tt, src, dst)
        }
    };

    // Resolve axelarId — consensus chains use a capitalised name on the Cosmos
    // side (e.g. "Avalanche" vs the JSON key "avalanche"). We keep the JSON
    // key for contract lookups but store the axelarId for verification queries.
    let source_axelar_id = chains
        .get(&source_chain)
        .and_then(|c| c.axelar_id.clone())
        .unwrap_or_else(|| source_chain.clone());
    let destination_axelar_id = chains
        .get(&destination_chain)
        .and_then(|c| c.axelar_id.clone())
        .unwrap_or_else(|| destination_chain.clone());

    // --- Read RPCs ---
    let source_rpc = chains
        .get(&source_chain)
        .and_then(|c| c.rpc.clone())
        .ok_or_else(|| eyre::eyre!("no RPC URL for source chain '{source_chain}' in config"))?;
    let destination_rpc = chains
        .get(&destination_chain)
        .and_then(|c| c.rpc.clone())
        .ok_or_else(|| {
            eyre::eyre!("no RPC URL for destination chain '{destination_chain}' in config")
        })?;

    let resolved_source_rpc = source_rpc_override.unwrap_or(source_rpc);
    let resolved_destination_rpc = destination_rpc_override.unwrap_or(destination_rpc);
    ui::kv("source RPC", &resolved_source_rpc);
    ui::kv("destination RPC", &resolved_destination_rpc);

    Ok(ResolvedConfig {
        test_type,
        source_chain,
        destination_chain,
        source_axelar_id,
        destination_axelar_id,
        source_rpc: resolved_source_rpc,
        destination_rpc: resolved_destination_rpc,
        private_key: private_key_override,
    })
}

/// Auto-detect source/destination chains for a known test type.
fn auto_detect_chains(
    chains: &HashMap<String, ChainEntry>,
    test_type: TestType,
    source_override: Option<String>,
    dest_override: Option<String>,
) -> Result<(String, String)> {
    match test_type {
        TestType::SolToEvm => {
            let source = match source_override {
                Some(s) => s,
                None => {
                    let svm = find_chains_by_type(chains, "svm", false);
                    match svm.len() {
                        0 => return Err(eyre::eyre!("no SVM (Solana) chain found in config")),
                        1 => {
                            ui::info(&format!("auto-detected source: {}", svm[0]));
                            svm[0].clone()
                        }
                        _ => {
                            return Err(eyre::eyre!(
                                "multiple SVM chains found: {}. Use --source-chain to pick one.",
                                svm.join(", ")
                            ));
                        }
                    }
                }
            };
            let dest = match dest_override {
                Some(d) => d,
                None => {
                    let evm = find_chains_by_type(chains, "evm", true);
                    if evm.is_empty() {
                        return Err(eyre::eyre!("no EVM chain found in config"));
                    }
                    ui::info(&format!(
                        "auto-detected destination: {} (use --destination-chain to override)",
                        evm[0]
                    ));
                    evm[0].clone()
                }
            };
            Ok((source, dest))
        }
        TestType::EvmToSol => {
            let source = match source_override {
                Some(s) => s,
                None => {
                    let evm = find_chains_by_type(chains, "evm", true);
                    match evm.len() {
                        0 => return Err(eyre::eyre!("no EVM chain found in config")),
                        1 => {
                            ui::info(&format!("auto-detected source: {}", evm[0]));
                            evm[0].clone()
                        }
                        _ => {
                            return Err(eyre::eyre!(
                                "multiple EVM chains found: {}. Use --source-chain to pick one.",
                                evm.join(", ")
                            ));
                        }
                    }
                }
            };
            let dest = match dest_override {
                Some(d) => d,
                None => {
                    let svm = find_chains_by_type(chains, "svm", false);
                    if svm.is_empty() {
                        return Err(eyre::eyre!("no SVM (Solana) chain found in config"));
                    }
                    ui::info(&format!(
                        "auto-detected destination: {} (use --destination-chain to override)",
                        svm[0]
                    ));
                    svm[0].clone()
                }
            };
            Ok((source, dest))
        }
        TestType::EvmToEvm => {
            let source = match source_override {
                Some(s) => s,
                None => {
                    let evm = find_chains_by_type(chains, "evm", true);
                    if evm.len() < 2 {
                        return Err(eyre::eyre!(
                            "need at least 2 EVM chains in config for evm-to-evm"
                        ));
                    }
                    ui::info(&format!("auto-detected source: {}", evm[0]));
                    evm[0].clone()
                }
            };
            let dest = match dest_override {
                Some(d) => d,
                None => {
                    let evm = find_chains_by_type(chains, "evm", true);
                    let picked = evm
                        .iter()
                        .find(|c| **c != source)
                        .ok_or_else(|| eyre::eyre!("need at least 2 EVM chains for evm-to-evm"))?;
                    ui::info(&format!(
                        "auto-detected destination: {} (use --destination-chain to override)",
                        picked
                    ));
                    picked.clone()
                }
            };
            Ok((source, dest))
        }
        TestType::SolToSol => {
            let source = match source_override {
                Some(s) => s,
                None => {
                    let svm = find_chains_by_type(chains, "svm", false);
                    if svm.is_empty() {
                        return Err(eyre::eyre!("no SVM (Solana) chain found in config"));
                    }
                    ui::info(&format!("auto-detected source: {}", svm[0]));
                    svm[0].clone()
                }
            };
            let dest = match dest_override {
                Some(d) => d,
                None => {
                    // For sol-to-sol, default to the same chain (loopback)
                    ui::info(&format!(
                        "auto-detected destination: {} (same as source for sol-to-sol)",
                        source
                    ));
                    source.clone()
                }
            };
            Ok((source, dest))
        }
        TestType::XrplToEvm => {
            let source = match source_override {
                Some(s) => s,
                None => {
                    let xrpl = find_chains_by_type(chains, "xrpl", false);
                    match xrpl.len() {
                        0 => return Err(eyre::eyre!("no XRPL chain found in config")),
                        1 => {
                            ui::info(&format!("auto-detected source: {}", xrpl[0]));
                            xrpl[0].clone()
                        }
                        _ => {
                            return Err(eyre::eyre!(
                                "multiple XRPL chains found: {}. Use --source-chain to pick one.",
                                xrpl.join(", ")
                            ));
                        }
                    }
                }
            };
            let dest = match dest_override {
                Some(d) => d,
                None => {
                    let evm = find_chains_by_type(chains, "evm", true);
                    if evm.is_empty() {
                        return Err(eyre::eyre!("no EVM chain found in config"));
                    }
                    ui::info(&format!(
                        "auto-detected destination: {} (use --destination-chain to override)",
                        evm[0]
                    ));
                    evm[0].clone()
                }
            };
            Ok((source, dest))
        }
        TestType::EvmToXrpl => {
            let source = match source_override {
                Some(s) => s,
                None => {
                    let evm = find_chains_by_type(chains, "evm", true);
                    match evm.len() {
                        0 => return Err(eyre::eyre!("no EVM chain found in config")),
                        1 => {
                            ui::info(&format!("auto-detected source: {}", evm[0]));
                            evm[0].clone()
                        }
                        _ => {
                            return Err(eyre::eyre!(
                                "multiple EVM chains found: {}. Use --source-chain to pick one.",
                                evm.join(", ")
                            ));
                        }
                    }
                }
            };
            let dest = match dest_override {
                Some(d) => d,
                None => {
                    let xrpl = find_chains_by_type(chains, "xrpl", false);
                    if xrpl.is_empty() {
                        return Err(eyre::eyre!("no XRPL chain found in config"));
                    }
                    ui::info(&format!(
                        "auto-detected destination: {} (use --destination-chain to override)",
                        xrpl[0]
                    ));
                    xrpl[0].clone()
                }
            };
            Ok((source, dest))
        }
        TestType::StellarToEvm
        | TestType::EvmToStellar
        | TestType::StellarToSol
        | TestType::SolToStellar => {
            auto_detect_stellar_pair(chains, test_type, source_override, dest_override)
        }
        TestType::SuiToEvm
        | TestType::EvmToSui
        | TestType::SuiToSol
        | TestType::SolToSui
        | TestType::SuiToStellar
        | TestType::StellarToSui
        | TestType::SuiToXrpl
        | TestType::XrplToSui => {
            // Sui as source/dest: just require both chains explicitly to keep
            // the auto-detect simple. The user knows which env they're on.
            let source = source_override
                .ok_or_else(|| eyre::eyre!("{test_type} requires --source-chain"))?;
            let dest = dest_override
                .ok_or_else(|| eyre::eyre!("{test_type} requires --destination-chain"))?;
            Ok((source, dest))
        }
    }
}

fn auto_detect_stellar_pair(
    chains: &HashMap<String, ChainEntry>,
    tt: TestType,
    src_override: Option<String>,
    dst_override: Option<String>,
) -> Result<(String, String)> {
    let (src_type, dst_type): (&str, &str) = match tt {
        TestType::StellarToEvm => ("stellar", "evm"),
        TestType::EvmToStellar => ("evm", "stellar"),
        TestType::StellarToSol => ("stellar", "svm"),
        TestType::SolToStellar => ("svm", "stellar"),
        _ => unreachable!(),
    };
    let skip_core_on_evm = |t: &str| t == "evm";
    let pick = |t: &str, override_: Option<String>, role: &str| -> Result<String> {
        if let Some(x) = override_ {
            return Ok(x);
        }
        let found = find_chains_by_type(chains, t, skip_core_on_evm(t));
        match found.len() {
            0 => Err(eyre::eyre!("no {} chain found in config", t)),
            1 => {
                ui::info(&format!("auto-detected {role}: {}", found[0]));
                Ok(found[0].clone())
            }
            _ => {
                ui::info(&format!(
                    "auto-detected {role}: {} (multiple available, pass --{role}-chain to override)",
                    found[0]
                ));
                Ok(found[0].clone())
            }
        }
    };
    let source = pick(src_type, src_override, "source")?;
    let dest = pick(dst_type, dst_override, "destination")?;
    Ok((source, dest))
}

/// Auto-detect test type and chains when nothing is specified.
/// Looks at what chain types exist in the config and picks the best match.
fn auto_detect_all(
    chains: &HashMap<String, ChainEntry>,
    source_override: Option<String>,
    dest_override: Option<String>,
) -> Result<(TestType, String, String)> {
    // If one chain is given, figure out the other
    if let Some(ref src) = source_override {
        let src_type = chain_type(chains, src)
            .ok_or_else(|| eyre::eyre!("source chain '{src}' not found in config"))?;
        if src_type == "svm" {
            let evm = find_chains_by_type(chains, "evm", true);
            let dst = dest_override.unwrap_or_else(|| {
                ui::info(&format!(
                    "auto-detected destination: {} (use --destination-chain to override)",
                    evm[0]
                ));
                evm[0].clone()
            });
            ui::info("inferred test type: sol-to-evm");
            return Ok((TestType::SolToEvm, src.clone(), dst));
        }
        if src_type == "evm" {
            let svm = find_chains_by_type(chains, "svm", false);
            if svm.is_empty() {
                return Err(eyre::eyre!(
                    "no SVM chain found in config to pair with EVM source"
                ));
            }
            let dst = dest_override.unwrap_or_else(|| {
                ui::info(&format!(
                    "auto-detected destination: {} (use --destination-chain to override)",
                    svm[0]
                ));
                svm[0].clone()
            });
            ui::info("inferred test type: evm-to-sol");
            return Ok((TestType::EvmToSol, src.clone(), dst));
        }
        return Err(eyre::eyre!(
            "cannot infer test type from source chain type '{src_type}'. Use --test-type to specify."
        ));
    }

    if let Some(ref dst) = dest_override {
        let dst_type = chain_type(chains, dst)
            .ok_or_else(|| eyre::eyre!("destination chain '{dst}' not found in config"))?;
        if dst_type == "evm" {
            let svm = find_chains_by_type(chains, "svm", false);
            if svm.is_empty() {
                return Err(eyre::eyre!(
                    "no SVM chain found in config to pair with EVM destination"
                ));
            }
            ui::info(&format!("auto-detected source: {}", svm[0]));
            ui::info("inferred test type: sol-to-evm");
            return Ok((TestType::SolToEvm, svm[0].clone(), dst.clone()));
        }
        if dst_type == "svm" {
            let evm = find_chains_by_type(chains, "evm", true);
            if evm.is_empty() {
                return Err(eyre::eyre!(
                    "no EVM chain found in config to pair with SVM destination"
                ));
            }
            ui::info(&format!("auto-detected source: {}", evm[0]));
            ui::info("inferred test type: evm-to-sol");
            return Ok((TestType::EvmToSol, evm[0].clone(), dst.clone()));
        }
        return Err(eyre::eyre!(
            "cannot infer test type from destination chain type '{dst_type}'. Use --test-type to specify."
        ));
    }

    // Nothing specified — look for valid combinations
    let svm = find_chains_by_type(chains, "svm", false);
    let evm = find_chains_by_type(chains, "evm", true);

    if !svm.is_empty() && !evm.is_empty() {
        ui::info(&format!(
            "auto-detected: {} -> {} (sol-to-evm)",
            svm[0], evm[0]
        ));
        return Ok((TestType::SolToEvm, svm[0].clone(), evm[0].clone()));
    }

    Err(eyre::eyre!(
        "cannot auto-detect test type from config. Use --test-type, or --source-chain + --destination-chain."
    ))
}
pub(crate) fn its_cache_path(src: &str, dst: &str) -> PathBuf {
    let data_dir = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("axe");
    data_dir.join(format!("its-load-test-{src}-{dst}.json"))
}

pub(crate) fn read_its_cache(src: &str, dst: &str) -> serde_json::Value {
    let path = its_cache_path(src, dst);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

pub(crate) fn save_its_cache(src: &str, dst: &str, cache: &serde_json::Value) -> Result<()> {
    let path = its_cache_path(src, dst);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(cache)?)?;
    Ok(())
}

/// Returns the network this binary was compiled for based on cargo features.
pub(crate) fn compiled_network() -> &'static str {
    if cfg!(feature = "mainnet") {
        "mainnet"
    } else if cfg!(feature = "testnet") {
        "testnet"
    } else if cfg!(feature = "stagenet") {
        "stagenet"
    } else {
        "devnet-amplifier"
    }
}

/// Try to detect the target network from the config file path.
/// Looks for known network names in the filename (e.g. "stagenet.json", "devnet-amplifier.json").
pub(crate) fn detect_network_from_config(config: &std::path::Path) -> Option<&'static str> {
    let name = config.file_stem()?.to_str()?;
    ["mainnet", "testnet", "stagenet", "devnet-amplifier"]
        .iter()
        .find(|&&network| name == network)
        .copied()
}
