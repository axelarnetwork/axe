pub mod evm_sender;
pub mod its_evm_to_sol;
pub mod its_evm_to_sol_with_data;
pub mod its_sol_to_evm;
pub mod keypairs;
pub mod metrics;
pub mod sol_sender;
mod sustained;
mod verify;

use std::path::PathBuf;
use std::time::Instant;

use alloy::{
    network::TransactionBuilder,
    primitives::{Address, Bytes},
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
    sol_types::SolValue,
};
use eyre::Result;
use serde_json::json;

use owo_colors::OwoColorize;

use crate::cosmos::read_axelar_contract_field;
use crate::evm::read_artifact_bytecode;
use crate::ui;
use crate::utils::read_contract_address;

use self::metrics::LoadTestReport;

/// Load test type (extensible for future directions).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum TestType {
    /// Solana -> EVM cross-chain load test
    SolToEvm,
    /// EVM -> Solana cross-chain load test
    EvmToSol,
    /// EVM -> EVM cross-chain load test
    EvmToEvm,
    /// Solana -> Solana cross-chain load test
    SolToSol,
}

impl std::fmt::Display for TestType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TestType::SolToEvm => write!(f, "sol-to-evm"),
            TestType::EvmToSol => write!(f, "evm-to-sol"),
            TestType::EvmToEvm => write!(f, "evm-to-evm"),
            TestType::SolToSol => write!(f, "sol-to-sol"),
        }
    }
}

/// Protocol: GMP (callContract), ITS (interchainTransfer), or ITS with data
/// (interchainTransfer that triggers a contract call on the destination).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Protocol {
    #[default]
    Gmp,
    Its,
    /// ITS interchainTransfer with data — sends tokens AND calls the memo
    /// program on the Solana destination chain.
    ItsWithData,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Protocol::Gmp => write!(f, "gmp"),
            Protocol::Its => write!(f, "its"),
            Protocol::ItsWithData => write!(f, "its-with-data"),
        }
    }
}

/// CLI arguments for the load test command.
pub struct LoadTestArgs {
    pub config: PathBuf,
    pub test_type: TestType,
    pub protocol: Protocol,
    pub destination_chain: String,
    pub source_chain: String,
    /// The `axelarId` for the source chain (used for Cosmos-side verification).
    pub source_axelar_id: String,
    /// The `axelarId` for the destination chain (used for Cosmos-side verification).
    pub destination_axelar_id: String,
    pub source_rpc: String,
    pub destination_rpc: String,
    pub private_key: Option<String>,
    pub num_txs: u64,
    pub keypair: Option<String>,
    pub payload: Option<String>,
    pub gas_value: Option<String>,
    pub token_id: Option<String>,
    pub tps: Option<u64>,
    pub duration_secs: Option<u64>,
    pub key_cycle: u64,
}

/// Cache file for storing SenderReceiver address per chain.
fn cache_path(axelar_id: &str) -> PathBuf {
    let data_dir = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("axe");
    data_dir.join(format!("load-test-{axelar_id}.json"))
}

fn read_cache(axelar_id: &str) -> serde_json::Value {
    let path = cache_path(axelar_id);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

fn save_cache(axelar_id: &str, cache: &serde_json::Value) -> Result<()> {
    let path = cache_path(axelar_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(cache)?)?;
    Ok(())
}

/// Look up a chain's `chainType` from the config.
fn chain_type(
    chains: &serde_json::Map<String, serde_json::Value>,
    chain_id: &str,
) -> Option<String> {
    chains
        .get(chain_id)?
        .get("chainType")?
        .as_str()
        .map(String::from)
}

/// Find chains by chainType, optionally skipping core-* prefixed chains.
fn find_chains_by_type(
    chains: &serde_json::Map<String, serde_json::Value>,
    chain_type_filter: &str,
    skip_core: bool,
) -> Vec<String> {
    chains
        .iter()
        .filter(|(k, v)| {
            v.get("chainType").and_then(|t| t.as_str()) == Some(chain_type_filter)
                && !(skip_core && k.starts_with("core-"))
        })
        .map(|(k, _)| k.clone())
        .collect()
}

/// Infer test type from source and destination chain types.
fn infer_test_type(source_type: &str, dest_type: &str) -> Result<TestType> {
    match (source_type, dest_type) {
        ("svm", "evm") => Ok(TestType::SolToEvm),
        ("evm", "svm") => Ok(TestType::EvmToSol),
        ("evm", "evm") => Ok(TestType::EvmToEvm),
        ("svm", "svm") => Ok(TestType::SolToSol),
        _ => Err(eyre::eyre!(
            "unsupported chain type combination: {source_type} -> {dest_type}. \
             Supported: svm -> evm, evm -> svm, evm -> evm, svm -> svm"
        )),
    }
}

/// Resolved configuration from the config JSON.
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
/// 1. `--test-type` only → auto-detect source and destination chains
/// 2. `--source-chain` + `--destination-chain` only → infer test type from chainType
/// 3. All three → validate consistency
pub fn resolve_from_config(
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
    let config_root: serde_json::Value = serde_json::from_str(&config_content)?;

    let chains = config_root
        .get("chains")
        .and_then(|v| v.as_object())
        .ok_or_else(|| eyre::eyre!("no 'chains' object in config"))?;

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
        .and_then(|v| v.get("axelarId"))
        .and_then(|v| v.as_str())
        .unwrap_or(&source_chain)
        .to_string();
    let destination_axelar_id = chains
        .get(&destination_chain)
        .and_then(|v| v.get("axelarId"))
        .and_then(|v| v.as_str())
        .unwrap_or(&destination_chain)
        .to_string();

    // --- Read RPCs ---
    let source_rpc = chains
        .get(&source_chain)
        .and_then(|v| v.get("rpc"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("no RPC URL for source chain '{source_chain}' in config"))?
        .to_string();
    let destination_rpc = chains
        .get(&destination_chain)
        .and_then(|v| v.get("rpc"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            eyre::eyre!("no RPC URL for destination chain '{destination_chain}' in config")
        })?
        .to_string();

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
    chains: &serde_json::Map<String, serde_json::Value>,
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
    }
}

/// Auto-detect test type and chains when nothing is specified.
/// Looks at what chain types exist in the config and picks the best match.
fn auto_detect_all(
    chains: &serde_json::Map<String, serde_json::Value>,
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

/// ITS cache file for storing token info per chain pair.
fn its_cache_path(src: &str, dst: &str) -> PathBuf {
    let data_dir = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("axe");
    data_dir.join(format!("its-load-test-{src}-{dst}.json"))
}

pub fn read_its_cache(src: &str, dst: &str) -> serde_json::Value {
    let path = its_cache_path(src, dst);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

pub fn save_its_cache(src: &str, dst: &str, cache: &serde_json::Value) -> Result<()> {
    let path = its_cache_path(src, dst);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(cache)?)?;
    Ok(())
}

/// Returns the network this binary was compiled for based on cargo features.
fn compiled_network() -> &'static str {
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
fn detect_network_from_config(config: &std::path::Path) -> Option<&'static str> {
    let name = config.file_stem()?.to_str()?;
    ["mainnet", "testnet", "stagenet", "devnet-amplifier"]
        .iter()
        .find(|&&network| name == network)
        .copied()
}

pub async fn run(args: LoadTestArgs) -> Result<()> {
    // Check for network mismatch between compiled binary and config
    if let Some(target_network) = detect_network_from_config(&args.config) {
        let compiled = compiled_network();
        if compiled != target_network {
            eyre::bail!(
                "binary was compiled for '{compiled}' but config targets '{target_network}'. \
                 Rebuild with:\n  cargo build --release --features {target_network} --no-default-features"
            );
        }
    }

    let run_start = Instant::now();

    ui::section(&format!(
        "Load Test ({}/{}): {} -> {}",
        args.protocol, args.test_type, args.source_chain, args.destination_chain
    ));

    // Block consensus chains that have no VotingVerifier — we can't verify them
    let src = &args.source_chain;
    let has_source_vv = read_axelar_contract_field(
        &args.config,
        &format!("/axelar/contracts/VotingVerifier/{src}/address"),
    )
    .is_ok();
    if !has_source_vv {
        eyre::bail!(
            "source chain '{src}' has no VotingVerifier in the config (consensus chain). \
             Load test verification requires an Amplifier chain with a VotingVerifier."
        );
    }

    match (args.protocol, args.test_type) {
        (Protocol::Gmp, TestType::SolToEvm) => run_sol_to_evm(args, run_start).await,
        (Protocol::Gmp, TestType::EvmToSol) => run_evm_to_sol(args, run_start).await,
        (Protocol::Gmp, TestType::EvmToEvm) => run_evm_to_evm(args, run_start).await,
        (Protocol::Gmp, TestType::SolToSol) => run_sol_to_sol(args, run_start).await,
        (Protocol::Its, TestType::EvmToSol) => its_evm_to_sol::run(args, run_start).await,
        (Protocol::Its, TestType::SolToEvm) => its_sol_to_evm::run(args, run_start).await,
        (Protocol::Its, TestType::EvmToEvm | TestType::SolToSol) => {
            eyre::bail!(
                "ITS {}->{} is not yet supported",
                args.source_chain,
                args.destination_chain
            )
        }
        (Protocol::ItsWithData, TestType::EvmToSol) => {
            its_evm_to_sol_with_data::run(args, run_start).await
        }
        (Protocol::ItsWithData, _) => {
            eyre::bail!("its-with-data only supports evm-to-sol currently")
        }
    }
}

async fn run_sol_to_evm(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let dest = &args.destination_chain;
    let src = &args.source_chain;

    let config_content = std::fs::read_to_string(&args.config)
        .map_err(|e| eyre::eyre!("failed to read config {}: {e}", args.config.display()))?;
    let config_root: serde_json::Value = serde_json::from_str(&config_content)?;

    let rpc_url = &args.destination_rpc;

    // Validate RPCs before doing any work
    validate_solana_rpc(&args.source_rpc).await?;
    validate_evm_rpc(rpc_url).await?;

    // Check that verification contracts exist for this chain pair before doing any work
    if read_axelar_contract_field(
        &args.config,
        &format!("/axelar/contracts/Gateway/{dest}/address"),
    )
    .is_err()
    {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — \
             verification would fail. Pick a chain that has a Gateway entry, e.g.:\n  {}",
            list_gateway_chains(&config_root).join(", ")
        );
    }

    ui::kv("source", src);
    ui::kv("destination", dest);

    let gateway_addr = read_contract_address(&args.config, dest, "AxelarGateway")?;
    let gas_service_addr = read_contract_address(&args.config, dest, "AxelarGasService")?;

    ui::address("EVM gateway", &format!("{gateway_addr}"));

    // --- Deploy/reuse SenderReceiver on destination EVM chain ---
    let cache = read_cache(dest);

    let (sender_receiver_addr, provider) = if let Some(addr_str) =
        cache.get("senderReceiverAddress").and_then(|v| v.as_str())
    {
        // Try to reuse cached address — only need a read-only provider for the check
        let read_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
        let addr: alloy::primitives::Address = addr_str.parse()?;
        let code = read_provider.get_code_at(addr).await?;
        // Check if code exists and gateway matches config
        let needs_redeploy = if code.is_empty() {
            ui::warn("cached SenderReceiver has no code, redeploying...");
            true
        } else {
            let sr = crate::evm::SenderReceiver::new(addr, &read_provider);
            match sr.gateway().call().await {
                Ok(onchain_gw) if onchain_gw != gateway_addr => {
                    ui::warn(&format!(
                        "cached SenderReceiver points to old gateway {onchain_gw}, expected {gateway_addr}, redeploying..."
                    ));
                    true
                }
                Err(_) => {
                    ui::warn("cached SenderReceiver gateway check failed, redeploying...");
                    true
                }
                _ => false,
            }
        };
        if needs_redeploy {
            let private_key = args.private_key.as_ref().ok_or_else(|| {
                eyre::eyre!("EVM private key required to deploy SenderReceiver. Set EVM_PRIVATE_KEY env var or use --private-key")
            })?;
            let signer: PrivateKeySigner = private_key.parse()?;
            check_evm_balance(&read_provider, signer.address()).await?;
            let write_provider = ProviderBuilder::new()
                .wallet(signer)
                .connect_http(rpc_url.parse()?);
            let new_addr =
                deploy_sender_receiver(&write_provider, gateway_addr, gas_service_addr).await?;
            let mut cache = cache;
            cache["senderReceiverAddress"] = json!(format!("{new_addr}"));
            save_cache(dest, &cache)?;
            (new_addr, write_provider)
        } else {
            ui::info(&format!("SenderReceiver: reusing {addr}"));
            let private_key = args
                .private_key
                .as_deref()
                .unwrap_or("0x0000000000000000000000000000000000000000000000000000000000000001");
            let signer: PrivateKeySigner = private_key.parse()?;
            let provider = ProviderBuilder::new()
                .wallet(signer)
                .connect_http(rpc_url.parse()?);
            (addr, provider)
        }
    } else {
        let private_key = args.private_key.as_ref().ok_or_else(|| {
            eyre::eyre!("EVM private key required to deploy SenderReceiver. Set EVM_PRIVATE_KEY env var or use --private-key")
        })?;
        let signer: PrivateKeySigner = private_key.parse()?;
        let deployer_addr = signer.address();
        let read_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
        check_evm_balance(&read_provider, deployer_addr).await?;
        ui::info("deploying SenderReceiver on destination chain...");
        let write_provider = ProviderBuilder::new()
            .wallet(signer)
            .connect_http(rpc_url.parse()?);
        let addr = deploy_sender_receiver(&write_provider, gateway_addr, gas_service_addr).await?;
        let mut cache = cache;
        cache["senderReceiverAddress"] = json!(format!("{addr}"));
        save_cache(dest, &cache)?;
        (addr, write_provider)
    };

    ui::address("SenderReceiver", &format!("{sender_receiver_addr}"));
    let destination_address = format!("{sender_receiver_addr}");

    let test_start = Instant::now();
    let mut report = if args.tps.is_some() && args.duration_secs.is_some() {
        {
            let (spinner_tx, _spinner_rx) =
                tokio::sync::oneshot::channel::<indicatif::ProgressBar>();
            sol_sender::run_sustained_load_test_with_metrics(
                &args,
                true,
                &destination_address,
                None,
                None,
                spinner_tx,
            )
            .await?
        }
    } else {
        sol_sender::run_load_test_with_metrics(&args, &destination_address, true).await?
    };

    let verification = verify::verify_onchain(
        &args.config,
        &args.source_axelar_id,
        &args.destination_axelar_id,
        &destination_address,
        gateway_addr,
        &provider,
        &mut report.transactions,
        verify::SourceChainType::Svm,
    )
    .await?;
    report.verification = Some(verification);

    finish_report(&args, &mut report, test_start)
}

async fn run_evm_to_sol(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let config_content = std::fs::read_to_string(&args.config)
        .map_err(|e| eyre::eyre!("failed to read config {}: {e}", args.config.display()))?;
    let config_root: serde_json::Value = serde_json::from_str(&config_content)?;

    let evm_rpc_url = args.source_rpc.clone();

    // Validate RPCs before doing any work
    validate_evm_rpc(&evm_rpc_url).await?;
    validate_solana_rpc(&args.destination_rpc).await?;

    // Check that verification contracts exist for this chain pair before doing any work
    if read_axelar_contract_field(
        &args.config,
        &format!("/axelar/contracts/Gateway/{dest}/address"),
    )
    .is_err()
    {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — \
             verification would fail. Pick a chain that has a Gateway entry, e.g.:\n  {}",
            list_gateway_chains(&config_root).join(", ")
        );
    }

    ui::kv("source", src);
    ui::kv("destination", dest);

    let gateway_addr = read_contract_address(&args.config, src, "AxelarGateway")?;
    let gas_service_addr =
        read_contract_address(&args.config, src, "AxelarGasService").unwrap_or(Address::ZERO);
    ui::address("EVM gateway", &format!("{gateway_addr}"));

    // --- Set up EVM signer ---
    let private_key = args.private_key.as_ref().ok_or_else(|| {
        eyre::eyre!("EVM private key required. Set EVM_PRIVATE_KEY env var or use --private-key")
    })?;
    let signer: PrivateKeySigner = private_key.parse()?;
    let signer_address = signer.address();
    let read_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    check_evm_balance(&read_provider, signer_address).await?;

    let write_provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(evm_rpc_url.parse()?);

    // Extract 32-byte private key for deriving sub-wallets
    let main_key: [u8; 32] = signer.to_bytes().into();

    #[allow(clippy::float_arithmetic)]
    {
        let balance: u128 = read_provider.get_balance(signer_address).await?.to();
        let eth = balance as f64 / 1e18;
        ui::kv("wallet", &format!("{signer_address} ({eth:.6} ETH)"));
    }

    // --- Deploy/reuse SenderReceiver on source chain ---
    let cache_key = &format!("{src}-evm-to-sol");
    let cache = read_cache(cache_key);
    let sender_receiver_addr = deploy_or_reuse_sender_receiver(
        &cache,
        cache_key,
        &read_provider,
        &write_provider,
        gateway_addr,
        gas_service_addr,
        "source",
    )
    .await?;
    ui::address("SenderReceiver", &format!("{sender_receiver_addr}"));

    // Destination on Solana: memo program (resolved per feature flag)
    let destination_address = evm_sender::memo_program_id().to_string();
    let destination_address = destination_address.as_str();
    ui::kv("destination program", destination_address);

    let test_start = Instant::now();
    let sustained = args.tps.is_some() && args.duration_secs.is_some();

    let mut report = if sustained {
        // Sustained mode: run verification concurrently with the send phase.
        let (verify_tx, verify_rx) = tokio::sync::mpsc::unbounded_channel();
        let send_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // The MultiProgress + spinners are created inside the send function AFTER
        // funding completes, so they don't flicker during setup. The verify spinner
        // is sent to the verify task via a oneshot channel.
        let (spinner_tx, spinner_rx) = tokio::sync::oneshot::channel::<indicatif::ProgressBar>();

        // Spawn verification in a background task.
        let vconfig = args.config.clone();
        let vsource = args.source_axelar_id.clone();
        let vdest = args.destination_axelar_id.clone();
        let vdest_addr = destination_address.to_string();
        let vdest_rpc = args.destination_rpc.clone();
        let vdone = std::sync::Arc::clone(&send_done);
        let verify_handle = tokio::spawn(async move {
            let spinner = spinner_rx.await.expect("spinner channel dropped");
            verify::verify_onchain_solana_streaming(
                &vconfig,
                &vsource,
                &vdest,
                &vdest_addr,
                &vdest_rpc,
                verify_rx,
                vdone,
                spinner,
            )
            .await
        });

        let mut report = evm_sender::run_sustained_load_test_with_metrics(
            &args,
            sender_receiver_addr,
            &main_key,
            &evm_rpc_url,
            destination_address,
            Some(verify_tx),
            Some(send_done),
            spinner_tx,
            false,
        )
        .await?;

        // Wait for verification to finish.
        let (verification, timings) = verify_handle.await??;
        // Write amplifier timing back into per-tx records for JSON report & pipeline counts.
        // Timings are keyed by message_id (signature); match them to transactions.
        for (msg_id, timing) in timings {
            if let Some(tx) = report.transactions.iter_mut().find(|t| {
                t.signature == msg_id
                    || format!(
                        "{}-{}.1",
                        t.signature,
                        crate::solana::solana_call_contract_index()
                    ) == msg_id
            }) {
                tx.amplifier_timing = Some(timing);
            }
        }
        report.verification = Some(verification);
        report
    } else {
        let mut report = evm_sender::run_load_test_with_metrics(
            &args,
            sender_receiver_addr,
            &main_key,
            &evm_rpc_url,
            destination_address,
            false,
        )
        .await?;

        let verification = verify::verify_onchain_solana(
            &args.config,
            &args.source_axelar_id,
            &args.destination_axelar_id,
            destination_address,
            &args.destination_rpc,
            &mut report.transactions,
            verify::SourceChainType::Evm,
        )
        .await?;
        report.verification = Some(verification);
        report
    };

    finish_report(&args, &mut report, test_start)
}

async fn run_evm_to_evm(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let config_content = std::fs::read_to_string(&args.config)
        .map_err(|e| eyre::eyre!("failed to read config {}: {e}", args.config.display()))?;
    let config_root: serde_json::Value = serde_json::from_str(&config_content)?;

    let source_rpc_url = args.source_rpc.clone();
    let dest_rpc_url = args.destination_rpc.clone();

    // Validate RPCs before doing any work
    validate_evm_rpc(&source_rpc_url).await?;
    validate_evm_rpc(&dest_rpc_url).await?;

    // Check that verification contracts exist
    if read_axelar_contract_field(
        &args.config,
        &format!("/axelar/contracts/Gateway/{dest}/address"),
    )
    .is_err()
    {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — \
             verification would fail. Pick a chain that has a Gateway entry, e.g.:\n  {}",
            list_gateway_chains(&config_root).join(", ")
        );
    }

    ui::kv("source", src);
    ui::kv("destination", dest);

    // --- Set up EVM signer ---
    let private_key = args.private_key.as_ref().ok_or_else(|| {
        eyre::eyre!("EVM private key required. Set EVM_PRIVATE_KEY env var or use --private-key")
    })?;
    let signer: PrivateKeySigner = private_key.parse()?;
    let signer_address = signer.address();
    let source_read_provider = ProviderBuilder::new().connect_http(source_rpc_url.parse()?);
    check_evm_balance(&source_read_provider, signer_address).await?;

    let source_write_provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(source_rpc_url.parse()?);

    let main_key: [u8; 32] = signer.to_bytes().into();

    #[allow(clippy::float_arithmetic)]
    {
        let balance: u128 = source_read_provider.get_balance(signer_address).await?.to();
        let eth = balance as f64 / 1e18;
        ui::kv("wallet", &format!("{signer_address} ({eth:.6} ETH)"));
    }

    // --- Source chain: deploy/reuse SenderReceiver (for sending) ---
    let src_gateway_addr = read_contract_address(&args.config, src, "AxelarGateway")?;
    let src_gas_service_addr =
        read_contract_address(&args.config, src, "AxelarGasService").unwrap_or(Address::ZERO);
    ui::address("source gateway", &format!("{src_gateway_addr}"));

    let src_cache_key = &format!("{src}-evm-to-evm");
    let src_cache = read_cache(src_cache_key);
    let sender_receiver_addr = deploy_or_reuse_sender_receiver(
        &src_cache,
        src_cache_key,
        &source_read_provider,
        &source_write_provider,
        src_gateway_addr,
        src_gas_service_addr,
        "source",
    )
    .await?;
    ui::address(
        "SenderReceiver (source)",
        &format!("{sender_receiver_addr}"),
    );

    // --- Destination chain: deploy/reuse SenderReceiver (as receive target) ---
    let dest_gateway_addr = read_contract_address(&args.config, dest, "AxelarGateway")?;
    let dest_gas_service_addr =
        read_contract_address(&args.config, dest, "AxelarGasService").unwrap_or(Address::ZERO);
    ui::address("destination gateway", &format!("{dest_gateway_addr}"));

    let dest_read_provider = ProviderBuilder::new().connect_http(dest_rpc_url.parse()?);
    let dest_write_provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(dest_rpc_url.parse()?);

    // Fund the signer on the destination chain if needed for SenderReceiver deployment
    let dest_balance: u128 = dest_read_provider.get_balance(signer_address).await?.to();
    if dest_balance == 0 {
        eyre::bail!(
            "EVM wallet {signer_address} has no funds on destination chain '{dest}'. \
             Fund it first."
        );
    }

    let dest_cache_key = &format!("{dest}-evm-to-evm-dest");
    let dest_cache = read_cache(dest_cache_key);
    let dest_sender_receiver = deploy_or_reuse_sender_receiver(
        &dest_cache,
        dest_cache_key,
        &dest_read_provider,
        &dest_write_provider,
        dest_gateway_addr,
        dest_gas_service_addr,
        "destination",
    )
    .await?;
    ui::address(
        "SenderReceiver (destination)",
        &format!("{dest_sender_receiver}"),
    );

    let destination_address = format!("{dest_sender_receiver}");

    let test_start = Instant::now();
    let mut report = if args.tps.is_some() && args.duration_secs.is_some() {
        evm_sender::run_sustained_load_test_with_metrics(
            &args,
            sender_receiver_addr,
            &main_key,
            &source_rpc_url,
            &destination_address,
            None,
            None,
            tokio::sync::oneshot::channel().0,
            true,
        )
        .await?
    } else {
        evm_sender::run_load_test_with_metrics(
            &args,
            sender_receiver_addr,
            &main_key,
            &source_rpc_url,
            &destination_address,
            true,
        )
        .await?
    };

    let verification = verify::verify_onchain(
        &args.config,
        &args.source_axelar_id,
        &args.destination_axelar_id,
        &destination_address,
        dest_gateway_addr,
        &dest_read_provider,
        &mut report.transactions,
        verify::SourceChainType::Evm,
    )
    .await?;
    report.verification = Some(verification);

    finish_report(&args, &mut report, test_start)
}

async fn run_sol_to_sol(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    // Validate RPCs
    validate_solana_rpc(&args.source_rpc).await?;
    validate_solana_rpc(&args.destination_rpc).await?;

    let config_content = std::fs::read_to_string(&args.config)
        .map_err(|e| eyre::eyre!("failed to read config {}: {e}", args.config.display()))?;
    let config_root: serde_json::Value = serde_json::from_str(&config_content)?;

    // Check that verification contracts exist
    if read_axelar_contract_field(
        &args.config,
        &format!("/axelar/contracts/Gateway/{dest}/address"),
    )
    .is_err()
    {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — \
             verification would fail. Pick a chain that has a Gateway entry, e.g.:\n  {}",
            list_gateway_chains(&config_root).join(", ")
        );
    }

    ui::kv("source", src);
    ui::kv("destination", dest);

    // Destination is the Solana memo program
    let destination_address = evm_sender::memo_program_id().to_string();
    let destination_address = destination_address.as_str();
    ui::kv("destination program", destination_address);

    let test_start = Instant::now();
    let sustained = args.tps.is_some() && args.duration_secs.is_some();

    let mut report = if sustained {
        // Sustained mode: run verification concurrently with the send phase.
        let (verify_tx, verify_rx) = tokio::sync::mpsc::unbounded_channel();
        let send_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (spinner_tx, spinner_rx) = tokio::sync::oneshot::channel::<indicatif::ProgressBar>();

        // Spawn verification in a background task.
        let vconfig = args.config.clone();
        let vsource = args.source_axelar_id.clone();
        let vdest = args.destination_axelar_id.clone();
        let vdest_addr = destination_address.to_string();
        let vdest_rpc = args.destination_rpc.clone();
        let vdone = std::sync::Arc::clone(&send_done);
        let verify_handle = tokio::spawn(async move {
            let spinner = spinner_rx.await.expect("spinner channel dropped");
            verify::verify_onchain_solana_streaming(
                &vconfig,
                &vsource,
                &vdest,
                &vdest_addr,
                &vdest_rpc,
                verify_rx,
                vdone,
                spinner,
            )
            .await
        });

        let mut report = sol_sender::run_sustained_load_test_with_metrics(
            &args,
            false,
            destination_address,
            Some(verify_tx),
            Some(send_done),
            spinner_tx,
        )
        .await?;

        // Wait for verification to finish.
        let (verification, timings) = verify_handle.await??;
        for (msg_id, timing) in timings {
            if let Some(tx) = report.transactions.iter_mut().find(|t| {
                t.signature == msg_id
                    || format!(
                        "{}-{}.1",
                        t.signature,
                        crate::solana::solana_call_contract_index()
                    ) == msg_id
            }) {
                tx.amplifier_timing = Some(timing);
            }
        }
        report.verification = Some(verification);
        report
    } else {
        let mut report =
            sol_sender::run_load_test_with_metrics(&args, destination_address, false).await?;

        let verification = verify::verify_onchain_solana(
            &args.config,
            &args.source_axelar_id,
            &args.destination_axelar_id,
            destination_address,
            &args.destination_rpc,
            &mut report.transactions,
            verify::SourceChainType::Svm,
        )
        .await?;
        report.verification = Some(verification);
        report
    };

    finish_report(&args, &mut report, test_start)
}

/// Deploy or reuse a cached SenderReceiver contract.
async fn deploy_or_reuse_sender_receiver<R: Provider, W: Provider>(
    cache: &serde_json::Value,
    cache_key: &str,
    read_provider: &R,
    write_provider: &W,
    gateway_addr: Address,
    gas_service_addr: Address,
    label: &str,
) -> Result<Address> {
    if let Some(addr_str) = cache.get("senderReceiverAddress").and_then(|v| v.as_str()) {
        let addr: Address = addr_str.parse()?;
        let code = read_provider.get_code_at(addr).await?;
        let needs_redeploy = if code.is_empty() {
            ui::warn(&format!(
                "cached SenderReceiver ({label}) has no code, redeploying..."
            ));
            true
        } else {
            // Verify the cached contract's gateway matches the current config.
            let sr = crate::evm::SenderReceiver::new(addr, read_provider);
            match sr.gateway().call().await {
                Ok(onchain_gw) => {
                    if onchain_gw != gateway_addr {
                        ui::warn(&format!(
                            "cached SenderReceiver ({label}) points to old gateway {onchain_gw}, expected {gateway_addr}, redeploying..."
                        ));
                        true
                    } else {
                        false
                    }
                }
                Err(_) => {
                    ui::warn(&format!(
                        "cached SenderReceiver ({label}) gateway check failed, redeploying..."
                    ));
                    true
                }
            }
        };
        if needs_redeploy {
            let new_addr =
                deploy_sender_receiver(write_provider, gateway_addr, gas_service_addr).await?;
            let mut cache = cache.clone();
            cache["senderReceiverAddress"] = json!(format!("{new_addr}"));
            save_cache(cache_key, &cache)?;
            Ok(new_addr)
        } else {
            ui::info(&format!("SenderReceiver ({label}): reusing {addr}"));
            Ok(addr)
        }
    } else {
        ui::info(&format!("deploying SenderReceiver on {label} chain..."));
        let addr = deploy_sender_receiver(write_provider, gateway_addr, gas_service_addr).await?;
        let mut cache = cache.clone();
        cache["senderReceiverAddress"] = json!(format!("{addr}"));
        save_cache(cache_key, &cache)?;
        Ok(addr)
    }
}

pub fn finish_report(
    args: &LoadTestArgs,
    report: &mut LoadTestReport,
    run_start: Instant,
) -> Result<()> {
    report.protocol = format!("{}", args.protocol);
    report.tps = args.tps;
    report.duration_secs = args.duration_secs;
    print_final_report(report);
    ui::success(&format!(
        "load test complete ({})",
        ui::format_elapsed(run_start)
    ));

    // Write full JSON report to a timestamped file so failures can be inspected afterwards.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let log_dir = std::path::Path::new("axe-load-test-logs");
    let log_path = log_dir.join(format!("axe-load-test-{ts}.json"));
    match std::fs::create_dir_all(log_dir) {
        Ok(()) => match serde_json::to_string_pretty(report) {
            Ok(json) => match std::fs::write(&log_path, &json) {
                Ok(()) => ui::info(&format!("report written to {}", log_path.display())),
                Err(e) => ui::warn(&format!(
                    "could not write report to {}: {e}",
                    log_path.display()
                )),
            },
            Err(e) => ui::warn(&format!("could not serialize report: {e}")),
        },
        Err(e) => ui::warn(&format!(
            "could not create log dir {}: {e}",
            log_dir.display()
        )),
    }

    Ok(())
}

/// List chain names that have a Cosmos Gateway address in the config.
fn list_gateway_chains(config_root: &serde_json::Value) -> Vec<String> {
    config_root
        .pointer("/axelar/contracts/Gateway")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter(|(_, v)| v.get("address").and_then(|a| a.as_str()).is_some())
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Validate that an RPC endpoint speaks EVM JSON-RPC (eth_chainId).
pub async fn validate_evm_rpc(rpc_url: &str) -> Result<()> {
    let provider = ProviderBuilder::new().connect_http(
        rpc_url
            .parse()
            .map_err(|e| eyre::eyre!("invalid RPC URL '{rpc_url}': {e}"))?,
    );
    provider.get_chain_id().await.map_err(|_| {
        eyre::eyre!(
            "RPC '{rpc_url}' does not appear to be an EVM endpoint \
             (eth_chainId failed). Check that you're using the correct RPC URL."
        )
    })?;
    Ok(())
}

/// Validate that an RPC endpoint speaks Solana JSON-RPC (getVersion).
pub async fn validate_solana_rpc(rpc_url: &str) -> Result<()> {
    let client = solana_client::nonblocking::rpc_client::RpcClient::new(rpc_url.to_string());
    client.get_version().await.map_err(|_| {
        eyre::eyre!(
            "RPC '{rpc_url}' does not appear to be a Solana endpoint \
             (getVersion failed). Check that you're using the correct RPC URL."
        )
    })?;
    Ok(())
}

pub async fn check_evm_balance<P: alloy::providers::Provider>(
    provider: &P,
    address: alloy::primitives::Address,
) -> Result<()> {
    let balance = provider.get_balance(address).await?;
    if balance.is_zero() {
        eyre::bail!(
            "EVM wallet {address} has no funds. Fund it first:\n  \
             Use a faucet or transfer native tokens to {address}"
        );
    }
    Ok(())
}

async fn deploy_sender_receiver<P: alloy::providers::Provider>(
    provider: &P,
    gateway: alloy::primitives::Address,
    gas_service: alloy::primitives::Address,
) -> Result<alloy::primitives::Address> {
    let bytecode = read_artifact_bytecode("artifacts/SenderReceiver.json")?;
    let mut deploy_code = bytecode;
    deploy_code.extend_from_slice(&(gateway, gas_service).abi_encode_params());

    let tx = TransactionRequest::default().with_deploy_code(Bytes::from(deploy_code));
    let pending = provider.send_transaction(tx).await?;
    let tx_hash = *pending.tx_hash();
    ui::tx_hash("deploy tx", &format!("{tx_hash}"));
    ui::info("waiting for confirmation...");

    let receipt = tokio::time::timeout(std::time::Duration::from_secs(120), pending.get_receipt())
        .await
        .map_err(|_| eyre::eyre!("deploy tx timed out after 120s"))??;

    let addr = receipt
        .contract_address
        .ok_or_else(|| eyre::eyre!("no contract address in receipt"))?;

    ui::success(&format!(
        "deployed in block {}",
        receipt.block_number.unwrap_or(0)
    ));
    Ok(addr)
}

#[allow(clippy::float_arithmetic)]
fn print_final_report(report: &LoadTestReport) {
    println!();
    println!(
        "\u{2550}\u{2550}\u{2550} SUMMARY \u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}"
    );
    // Protocol + mode line
    {
        let mut mode_parts = vec![report.protocol.to_uppercase()];
        if let (Some(tps), Some(dur)) = (report.tps, report.duration_secs) {
            mode_parts.push(format!("{tps} tx/s"));
            mode_parts.push(format!("{dur}s"));
        }
        println!(
            "  {} -> {}  ({})",
            report.source_chain,
            report.destination_chain,
            mode_parts.join(", "),
        );
    }
    println!(
        "  transactions     {}/{} confirmed ({:.1}% landed)",
        report.total_confirmed,
        report.total_submitted,
        report.landing_rate * 100.0,
    );

    if let Some(ref v) = report.verification {
        println!();

        // Per-stage pipeline counts (computed from transaction records).
        // Shown even when verification times out so partial progress is visible.
        let total = report.total_confirmed;
        let voted = report
            .transactions
            .iter()
            .filter(|t| {
                t.amplifier_timing
                    .as_ref()
                    .is_some_and(|a| a.voted_secs.is_some())
            })
            .count() as u64;
        let routed = report
            .transactions
            .iter()
            .filter(|t| {
                t.amplifier_timing
                    .as_ref()
                    .is_some_and(|a| a.routed_secs.is_some())
            })
            .count() as u64;
        let approved = report
            .transactions
            .iter()
            .filter(|t| {
                t.amplifier_timing
                    .as_ref()
                    .is_some_and(|a| a.approved_secs.is_some())
            })
            .count() as u64;
        let executed = report
            .transactions
            .iter()
            .filter(|t| {
                t.amplifier_timing
                    .as_ref()
                    .is_some_and(|a| a.executed_secs.is_some())
            })
            .count() as u64;
        {
            let mut parts = Vec::new();
            // Skip voted for consensus chains (no VotingVerifier → always 0).
            if voted > 0 {
                parts.push(format!("voted {voted}/{total}"));
            }
            parts.push(format!("routed {routed}/{total}"));
            parts.push(format!("approved {approved}/{total}"));
            parts.push(format!("executed {executed}/{total}"));
            println!("  pipeline         {}", parts.join("  "));
        }

        // End-to-end line
        match (
            v.avg_executed_secs,
            v.min_executed_secs,
            v.max_executed_secs,
        ) {
            (Some(avg), Some(min), Some(max)) => {
                println!(
                    "  end-to-end       avg {avg:.1}s \u{2502} min {min:.1}s \u{2502} max {max:.1}s"
                );
            }
            (Some(avg), _, Some(max)) => {
                println!("  end-to-end       avg {avg:.1}s \u{2502} max {max:.1}s");
            }
            (Some(avg), _, _) => {
                println!("  end-to-end       avg {avg:.1}s");
            }
            _ => {}
        }

        // Sustained throughput per pipeline step (tx/s over each step's full span).
        {
            let p = &v.peak_throughput;
            let mut parts = Vec::new();
            if let Some(t) = p.voted_tps {
                parts.push(("voted", t));
            }
            if let Some(t) = p.routed_tps {
                parts.push(("routed", t));
            }
            if let Some(t) = p.hub_approved_tps {
                parts.push(("hub approved", t));
            }
            if let Some(t) = p.approved_tps {
                parts.push(("approved", t));
            }
            if let Some(t) = p.executed_tps {
                parts.push(("executed", t));
            }
            if !parts.is_empty() {
                println!("  throughput (sustained, tx/s)");
                for (name, rate) in &parts {
                    println!("    {name:<14} {rate:.1}");
                }
            }
        }

        // Latency percentiles (end-to-end, send → executed).
        {
            let mut latencies: Vec<f64> = report
                .transactions
                .iter()
                .filter_map(|t| t.amplifier_timing.as_ref()?.executed_secs)
                .collect();
            latencies.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let n = latencies.len();
            if n > 0 {
                let pct = |p: f64| -> f64 {
                    let idx = ((n as f64 * p) as usize).min(n - 1);
                    latencies[idx]
                };
                println!(
                    "  latency          p50 {:.1}s │ p90 {:.1}s │ p99 {:.1}s",
                    pct(0.50),
                    pct(0.90),
                    pct(0.99),
                );
            }
        }

        // Segment breakdown — show step duration + cumulative total.
        // Each avg_*_secs value is cumulative from tx send. Step = current - previous.
        // Steps are sorted by cumulative time so ITS hub_approved (which can precede
        // routed) displays in the correct chronological order.
        let src = &report.source_chain;
        let dst = &report.destination_chain;

        // Collect all available steps with their cumulative totals and labels.
        let mut steps: Vec<(f64, &str, String)> = Vec::new();
        if let Some(total) = report.avg_latency_ms.map(|ms| ms / 1000.0) {
            steps.push((total, "confirm", format!("({src})")));
        }
        if let Some(total) = v.avg_voted_secs {
            steps.push((total, "voted", "(axelar)".to_string()));
        }
        if let Some(total) = v.avg_routed_secs {
            steps.push((total, "routed", "(axelar)".to_string()));
        }
        if let Some(total) = v.avg_hub_approved_secs {
            steps.push((total, "hub approved", "(axelar hub)".to_string()));
        }
        if let Some(total) = v.avg_approved_secs {
            steps.push((total, "approved", format!("({dst})")));
        }
        if let Some(total) = v.avg_executed_secs {
            steps.push((total, "executed", format!("({dst})")));
        }

        // Sort by cumulative time so steps display in chronological order.
        steps.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        let mut prev: Option<f64> = None;
        for (i, (total, name, location)) in steps.iter().enumerate() {
            let step = prev.map_or(*total, |p| total - p);
            let is_last = i == steps.len() - 1;
            let connector = if is_last {
                "\u{2514}\u{2500}"
            } else {
                "\u{251c}\u{2500}"
            };
            println!(
                "  {} step {step:.1}s \u{2502} total {total:.1}s  {}",
                format!("{connector} {name:<13}").dimmed(),
                location.dimmed(),
            );
            prev = Some(*total);
        }

        // Stuck
        if v.stuck > 0 {
            let stuck_detail: Vec<String> = v
                .stuck_at
                .iter()
                .map(|c| format!("{} at {}", c.count, c.reason))
                .collect();
            println!();
            println!(
                "  stuck            {}/{} ({:.1}%) \u{2014} {}",
                v.stuck,
                v.total_verified,
                if v.total_verified > 0 {
                    v.stuck as f64 / v.total_verified as f64 * 100.0
                } else {
                    0.0
                },
                stuck_detail.join(", "),
            );
        }

        // Failures
        println!("  failures         {}", v.failed - v.stuck,);
        for cat in &v.failure_reasons {
            if !cat.reason.contains("timed out") {
                println!("                   {} \u{00d7} {}", cat.count, cat.reason);
            }
        }
    }
    println!();
}
