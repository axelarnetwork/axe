use std::path::{Path, PathBuf};
use std::time::Duration;

use alloy::consensus::Transaction;
use alloy::hex;
use alloy::primitives::{B256, TxHash};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionReceipt;
use eyre::{Result, bail};
use futures::future::join_all;
use owo_colors::OwoColorize;

use super::decode;

type RpcTx = alloy::rpc::types::Transaction;

const CONFIG_NAMES: &[&str] = &[
    "mainnet.json",
    "testnet.json",
    "stagenet.json",
    "devnet-amplifier.json",
];

fn discover_configs() -> Vec<PathBuf> {
    // Look for sibling axelar-contract-deployments repo
    let exe = std::env::current_exe().ok();
    let candidates: Vec<PathBuf> = [
        // relative to cwd
        Some(PathBuf::from("../axelar-contract-deployments")),
        // relative to the binary
        exe.as_ref()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .map(|p| p.join("axelar-contract-deployments")),
    ]
    .into_iter()
    .flatten()
    .collect();

    let mut configs = Vec::new();
    for base in candidates {
        let info_dir = base.join("axelar-chains-config/info");
        if info_dir.is_dir() {
            for name in CONFIG_NAMES {
                let path = info_dir.join(name);
                if path.exists() {
                    configs.push(path);
                }
            }
            if !configs.is_empty() {
                return configs;
            }
        }
    }
    configs
}

pub async fn run(txid: &str, config: Option<&Path>, chain_filter: Option<&str>) -> Result<()> {
    // Detect Solana vs EVM: Solana signatures are base58, ~88 chars, no 0x prefix
    if !txid.starts_with("0x") && txid.len() > 60 {
        // Likely a Solana signature
        let solana_rpc = resolve_solana_rpc(config);
        return super::decode_sol_tx::run(txid, &solana_rpc).await;
    }

    let tx_hash: TxHash = txid.parse().map_err(|_| eyre::eyre!("invalid tx hash"))?;

    let configs: Vec<PathBuf> = if let Some(c) = config {
        vec![c.to_path_buf()]
    } else {
        let found = discover_configs();
        if found.is_empty() {
            bail!(
                "no chains config found. Place axelar-contract-deployments as a sibling repo, \
                 or pass --config <path>"
            );
        }
        found
    };

    // Collect RPCs from all configs, dedup by RPC URL
    let mut rpcs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for cfg in &configs {
        for chain in load_evm_rpcs(cfg, chain_filter)? {
            if seen.insert(chain.rpc.clone()) {
                rpcs.push(chain);
            }
        }
    }

    if rpcs.is_empty() {
        bail!("no EVM chains found in config(s)");
    }

    println!("Searching {} EVM chains...", rpcs.len());
    let (chain_name, tx, receipt) = fetch_tx(&rpcs, tx_hash).await?;

    // Print summary
    println!("{:<8} {}", "Chain:".bold(), chain_name);
    println!("{:<8} {txid}", "Tx:".bold());
    if let Some(block) = tx.block_number {
        println!("{:<8} {block}", "Block:".bold());
    }
    println!("{:<8} {}", "From:".bold(), tx.inner.signer());
    if let Some(to) = tx.inner.to() {
        println!("{:<8} {to}", "To:".bold());
    }
    if !tx.inner.value().is_zero() {
        println!("{:<8} {} wei", "Value:".bold(), tx.inner.value());
    }
    if let Some(ref r) = receipt {
        let status = if r.status() {
            "Success".green().to_string()
        } else {
            "Failed".red().to_string()
        };
        println!("{:<8} {status}", "Status:".bold());
    } else {
        println!(
            "{:<8} {}",
            "Status:".bold(),
            "(receipt unavailable)".dimmed()
        );
    }

    // Decode calldata
    let input = tx.inner.input();
    if !input.is_empty() {
        println!("\n{}", "━━ Calldata ━━".bold());
        if let Err(e) = decode::decode_bytes(input, "") {
            println!("  Could not decode: {e}");
            let h = hex::encode(input);
            if h.len() > 128 {
                println!("  Raw: 0x{}… ({} bytes)", &h[..64], input.len());
            } else {
                println!("  Raw: 0x{h}");
            }
        }
    }

    // Decode logs
    if let Some(ref receipt) = receipt {
        let logs = receipt.inner.logs();
        if !logs.is_empty() {
            println!("\n{}", format!("━━ Logs ({}) ━━", logs.len()).bold());
            for (i, log) in logs.iter().enumerate() {
                let addr = log.address();
                let topics: Vec<B256> = log.topics().to_vec();
                let data = log.data().data.as_ref();

                println!("\n[{i}] {}", addr.dimmed());

                if topics.is_empty() {
                    println!("    (anonymous event, {} bytes data)", data.len());
                    continue;
                }

                match decode::decode_log(&topics, data) {
                    Some((sig, params)) => {
                        println!("    {}", sig.bold());
                        for (name, value) in &params {
                            println!("      {name}: {}", decode::format_value_pub(value));
                        }
                        // Try nested decode on bytes params
                        for (_, value) in &params {
                            if let alloy::dyn_abi::DynSolValue::Bytes(b) = value
                                && b.len() >= 4
                            {
                                let _ = decode::decode_bytes(b, "        ");
                            }
                        }
                    }
                    None => {
                        println!(
                            "    Unknown event (topic0: 0x{}…)",
                            hex::encode(&topics[0][..8])
                        );
                        if !data.is_empty() {
                            let h = hex::encode(data);
                            if h.len() > 128 {
                                println!("    data: 0x{}… ({} bytes)", &h[..64], data.len());
                            } else {
                                println!("    data: 0x{h}");
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

struct EvmChain {
    name: String,
    rpc: String,
}

fn alchemy_slug(chain_id: u64) -> Option<&'static str> {
    Some(match chain_id {
        1 => "eth-mainnet",
        11155111 => "eth-sepolia",
        137 => "polygon-mainnet",
        80002 => "polygon-amoy",
        42161 => "arb-mainnet",
        421614 => "arb-sepolia",
        10 => "opt-mainnet",
        11155420 => "opt-sepolia",
        8453 => "base-mainnet",
        84532 => "base-sepolia",
        43114 => "avax-mainnet",
        43113 => "avax-fuji",
        56 => "bnb-mainnet",
        97 => "bnb-testnet",
        59144 => "linea-mainnet",
        59141 => "linea-sepolia",
        534352 => "scroll-mainnet",
        534351 => "scroll-sepolia",
        81457 => "blast-mainnet",
        168587773 => "blast-sepolia",
        252 => "frax-mainnet",
        2522 => "frax-sepolia",
        1284 => "moonbeam-mainnet",
        250 => "fantom-mainnet",
        42220 => "celo-mainnet",
        5000 => "mantle-mainnet",
        80094 => "berachain-mainnet",
        747 => "flow-mainnet",
        545 => "flow-testnet",
        _ => return None,
    })
}

fn load_evm_rpcs(config: &Path, chain_filter: Option<&str>) -> Result<Vec<EvmChain>> {
    let content = std::fs::read_to_string(config)
        .map_err(|e| eyre::eyre!("failed to read config {}: {e}", config.display()))?;
    let root: serde_json::Value = serde_json::from_str(&content)?;
    let chains = root
        .get("chains")
        .and_then(|v| v.as_object())
        .ok_or_else(|| eyre::eyre!("no 'chains' object in config"))?;

    let alchemy_token = std::env::var("ALCHEMY_TOKEN").ok();

    let mut result = Vec::new();
    for (key, value) in chains {
        let chain_type = value
            .get("chainType")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if chain_type != "evm" {
            continue;
        }

        let axelar_id = value
            .get("axelarId")
            .and_then(|v| v.as_str())
            .unwrap_or(key);

        if let Some(filter) = chain_filter
            && axelar_id != filter
            && key != filter
        {
            continue;
        }

        let chain_id = value.get("chainId").and_then(|v| v.as_u64());

        // Prefer Alchemy RPC when token is available and chain is supported
        let rpc = alchemy_token
            .as_deref()
            .and_then(|token| {
                chain_id
                    .and_then(alchemy_slug)
                    .map(|slug| format!("https://{slug}.g.alchemy.com/v2/{token}"))
            })
            .or_else(|| value.get("rpc").and_then(|v| v.as_str()).map(String::from));

        if let Some(rpc) = rpc {
            result.push(EvmChain {
                name: axelar_id.to_string(),
                rpc,
            });
        }
    }

    Ok(result)
}

async fn fetch_tx(
    rpcs: &[EvmChain],
    tx_hash: TxHash,
) -> Result<(String, RpcTx, Option<TransactionReceipt>)> {
    let futures: Vec<_> = rpcs
        .iter()
        .map(|chain| {
            let rpc = chain.rpc.clone();
            let name = chain.name.clone();
            async move {
                let Ok(url) = rpc.parse() else { return None };
                let provider = ProviderBuilder::new().connect_http(url);
                let result = tokio::time::timeout(Duration::from_secs(15), async {
                    let tx = match provider.get_transaction_by_hash(tx_hash).await {
                        Ok(Some(tx)) => tx,
                        Ok(None) => return Err(eyre::eyre!("tx not found")),
                        Err(e) => return Err(eyre::eyre!("tx fetch: {e}")),
                    };
                    let receipt = match provider.get_transaction_receipt(tx_hash).await {
                        Ok(Some(r)) => Some(r),
                        Ok(None) | Err(_) => None,
                    };
                    Ok((tx, receipt))
                })
                .await;
                match result {
                    Ok(Ok((tx, receipt))) => Some((name, tx, receipt)),
                    _ => None,
                }
            }
        })
        .collect();

    let results = join_all(futures).await;

    if let Some((name, tx, receipt)) = results.into_iter().flatten().next() {
        return Ok((name, tx, receipt));
    }

    bail!(
        "transaction not found on any chain (tried {} RPCs)",
        rpcs.len()
    )
}

/// Resolve a Solana RPC URL from the config, falling back to devnet.
fn resolve_solana_rpc(config: Option<&Path>) -> String {
    const FALLBACK: &str = "https://api.devnet.solana.com";
    let Some(cfg_path) = config else {
        return FALLBACK.to_string();
    };
    let Ok(cfg) = crate::config::ChainsConfig::load(cfg_path) else {
        return FALLBACK.to_string();
    };
    cfg.chains
        .values()
        .find(|c| c.chain_type.as_deref() == Some("svm"))
        .and_then(|c| c.rpc.clone())
        .unwrap_or_else(|| FALLBACK.to_string())
}
