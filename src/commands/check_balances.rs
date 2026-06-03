//! `axe check-balances <network>` — pre-flight wallet balance check.
//!
//! Mirrors the (now-removed) `axelar-contract-deployments/scripts/
//! check-wallet-balances.js` from PR #1383: derives each chain's wallet
//! address from the corresponding env key, queries the native-token balance,
//! and exits non-zero if any wallet is below its per-chain threshold.
//!
//! Designed as a fail-fast pre-step for the cron amplifier-routes workflows:
//! if a wallet is underfunded, every load-test route that uses it will fail
//! mid-run with a confusing on-chain error; checking up front saves CI time
//! and produces a clear "fund this address" message in the first job.

use std::path::PathBuf;

use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use comfy_table::{Attribute, Cell, Color, ContentArrangement, Table};
use eyre::{Result, WrapErr, eyre};
use solana_sdk::signature::Signer;

use crate::config::{ChainConfig, ChainsConfig};
use crate::solana::{load_keypair, rpc_client};
use crate::stellar::{StellarClient, StellarWallet};
use crate::sui::{SuiClient, SuiWallet};
use crate::types::Network;
use crate::ui;
use crate::xrpl::{XrplClient, XrplWallet};

/// One chain's preflight target. Owned strings only — see
/// CLAUDE.md "Owned types in bundle structs".
#[derive(Clone, Debug)]
struct ChainTarget {
    chain_key: String,
    kind: ChainKind,
    threshold_units: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChainKind {
    Evm,
    Solana,
    Sui,
    Stellar,
    Xrpl,
}

impl ChainKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Evm => "EVM",
            Self::Solana => "Solana",
            Self::Sui => "Sui",
            Self::Stellar => "Stellar",
            Self::Xrpl => "XRPL",
        }
    }

    const fn default_symbol(self) -> &'static str {
        match self {
            Self::Evm => "ETH",
            Self::Solana => "SOL",
            Self::Sui => "SUI",
            Self::Stellar => "XLM",
            Self::Xrpl => "XRP",
        }
    }
}

#[derive(Clone, Debug)]
struct BalanceRow {
    chain_key: String,
    kind: ChainKind,
    address: Option<String>,
    balance: Option<f64>,
    threshold: f64,
    token_symbol: String,
    note: String,
}

impl BalanceRow {
    fn is_underfunded(&self) -> bool {
        self.balance.is_none_or(|b| b < self.threshold)
    }
}

/// Per-network chain → threshold (in native token units).
///
/// Values mirror the removed JS script's `THRESHOLDS` map verbatim, except
/// Solana (not present in the JS) is set to 0.3 SOL — large enough to cover
/// many GMP-fleet transactions including PDA-creating rent-exempt deposits.
fn chain_targets(network: Network) -> Vec<ChainTarget> {
    let stellar_key = match network {
        Network::Mainnet => "stellar",
        Network::Testnet | Network::Stagenet | Network::DevnetAmplifier => "stellar-2026-q1-2",
    };
    vec![
        ChainTarget {
            chain_key: "hyperliquid".to_string(),
            kind: ChainKind::Evm,
            threshold_units: 0.01,
        },
        ChainTarget {
            chain_key: "xrpl-evm".to_string(),
            kind: ChainKind::Evm,
            threshold_units: 2.0,
        },
        ChainTarget {
            chain_key: "xrpl".to_string(),
            kind: ChainKind::Xrpl,
            threshold_units: 3.0,
        },
        ChainTarget {
            chain_key: stellar_key.to_string(),
            kind: ChainKind::Stellar,
            threshold_units: 0.5,
        },
        ChainTarget {
            chain_key: "sui".to_string(),
            kind: ChainKind::Sui,
            threshold_units: 0.05,
        },
        ChainTarget {
            chain_key: "solana".to_string(),
            kind: ChainKind::Solana,
            threshold_units: 0.3,
        },
    ]
}

pub async fn run(network: String) -> Result<()> {
    let network: Network = network.parse()?;
    let config_path = resolve_config(network)?;
    let config = ChainsConfig::load(&config_path)?;

    ui::section(&format!("wallet balance check: {network}"));
    ui::kv("config", &config_path.display().to_string());

    let targets = chain_targets(network);
    let mut rows = Vec::with_capacity(targets.len());
    for target in targets {
        let row = probe_row(&config, &target, network).await;
        rows.push(row);
    }

    render_table(&rows);

    let underfunded: Vec<&BalanceRow> = rows.iter().filter(|r| r.is_underfunded()).collect();
    if !underfunded.is_empty() {
        ui::error(&format!(
            "{} wallet(s) below minimum threshold or unreachable:",
            underfunded.len()
        ));
        for row in &underfunded {
            ui::error(&format!("  {}", format_underfunded(row)));
        }
        return Err(eyre!(
            "wallet preflight failed — fund the addresses above (or fix the listed errors) and retry"
        ));
    }

    ui::success("all wallets above minimum thresholds");
    Ok(())
}

fn resolve_config(network: Network) -> Result<PathBuf> {
    let config_dir = PathBuf::from("../axelar-contract-deployments/axelar-chains-config/info");
    let path = config_dir.join(format!("{network}.json"));
    if !path.exists() {
        return Err(eyre!(
            "config not found for network '{}' at {}. \
             Make sure axelar-contract-deployments is a sibling directory.",
            network,
            path.display()
        ));
    }
    Ok(path)
}

async fn probe_row(config: &ChainsConfig, target: &ChainTarget, network: Network) -> BalanceRow {
    let token_symbol = config
        .chains
        .get(&target.chain_key)
        .and_then(|c| c.token_symbol.clone())
        .unwrap_or_else(|| target.kind.default_symbol().to_string());

    match probe_balance(config, target, network).await {
        Ok((address, balance)) => BalanceRow {
            chain_key: target.chain_key.clone(),
            kind: target.kind,
            address: Some(address),
            balance: Some(balance),
            threshold: target.threshold_units,
            token_symbol,
            note: String::new(),
        },
        Err(err) => BalanceRow {
            chain_key: target.chain_key.clone(),
            kind: target.kind,
            address: None,
            balance: None,
            threshold: target.threshold_units,
            token_symbol,
            note: shorten_error(&err.to_string()),
        },
    }
}

async fn probe_balance(
    config: &ChainsConfig,
    target: &ChainTarget,
    network: Network,
) -> Result<(String, f64)> {
    let chain = config
        .chains
        .get(&target.chain_key)
        .ok_or_else(|| eyre!("chain '{}' not in {network} config", target.chain_key))?;
    match target.kind {
        ChainKind::Evm => probe_evm(chain).await,
        ChainKind::Solana => probe_solana(chain).await,
        ChainKind::Sui => probe_sui(chain).await,
        ChainKind::Stellar => probe_stellar(chain, network).await,
        ChainKind::Xrpl => probe_xrpl(chain).await,
    }
}

async fn probe_evm(chain: &ChainConfig) -> Result<(String, f64)> {
    let rpc_url = chain
        .rpc
        .as_deref()
        .ok_or_else(|| eyre!("no rpc in config"))?;
    let key = std::env::var("EVM_PRIVATE_KEY").wrap_err("EVM_PRIVATE_KEY env not set")?;
    let signer: PrivateKeySigner = key
        .parse()
        .wrap_err("EVM_PRIVATE_KEY is not a valid hex private key")?;
    let addr: Address = signer.address();
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let wei = provider.get_balance(addr).await?;
    Ok((format!("{addr:#x}"), wei_to_eth(wei)))
}

async fn probe_solana(chain: &ChainConfig) -> Result<(String, f64)> {
    let rpc_url = chain
        .rpc
        .as_deref()
        .ok_or_else(|| eyre!("no rpc in config"))?;
    // SOLANA_PRIVATE_KEY (when set) is a file path written by the workflow's
    // composite action. Falls back to ~/.config/solana/id.json otherwise.
    let key_path = std::env::var("SOLANA_PRIVATE_KEY").ok();
    let keypair = load_keypair(key_path.as_deref())?;
    let pubkey = keypair.pubkey();
    let client = rpc_client(rpc_url);
    let lamports = client
        .get_balance(&pubkey)
        .map_err(|e| eyre!("get_balance({pubkey}): {e}"))?;
    Ok((pubkey.to_string(), lamports as f64 / 1_000_000_000.0))
}

async fn probe_sui(chain: &ChainConfig) -> Result<(String, f64)> {
    let rpc_url = chain
        .rpc
        .as_deref()
        .ok_or_else(|| eyre!("no rpc in config"))?;
    let key = std::env::var("SUI_PRIVATE_KEY").wrap_err("SUI_PRIVATE_KEY env not set")?;
    let wallet = SuiWallet::from_secret_str(&key)?;
    let client = SuiClient::new(rpc_url);
    let mist = client.get_balance(&wallet.address).await?;
    Ok((wallet.address_hex(), mist as f64 / 1_000_000_000.0))
}

async fn probe_stellar(chain: &ChainConfig, network: Network) -> Result<(String, f64)> {
    let rpc_url = chain
        .rpc
        .as_deref()
        .ok_or_else(|| eyre!("no rpc in config"))?;
    let network_type = stellar_network_type(chain, network);
    let client = StellarClient::new(rpc_url, &network_type)?;
    let key = std::env::var("STELLAR_PRIVATE_KEY").wrap_err("STELLAR_PRIVATE_KEY env not set")?;
    let wallet = StellarWallet::from_secret_str(&key)?;
    let address = wallet.address();
    let stroops = client
        .native_balance_stroops(&address)
        .await?
        .ok_or_else(|| eyre!("account {address} not found on Stellar"))?;
    // 1 XLM = 10^7 stroops
    Ok((address, stroops as f64 / 10_000_000.0))
}

async fn probe_xrpl(chain: &ChainConfig) -> Result<(String, f64)> {
    let rpc_url = chain
        .rpc
        .as_deref()
        .ok_or_else(|| eyre!("no rpc in config"))?;
    let key = std::env::var("XRPL_PRIVATE_KEY").wrap_err("XRPL_PRIVATE_KEY env not set")?;
    let wallet = XrplWallet::from_secret_str(&key)?;
    let address = wallet.address();
    let client = XrplClient::new(rpc_url);
    let info = client
        .account_info(&address)
        .await?
        .ok_or_else(|| eyre!("account {address} not activated on XRPL"))?;
    // 1 XRP = 10^6 drops
    Ok((address, info.balance_drops as f64 / 1_000_000.0))
}

fn stellar_network_type(chain: &ChainConfig, network: Network) -> String {
    chain
        .extra
        .get("networkType")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| match network {
            Network::Mainnet => "mainnet".to_string(),
            Network::Testnet | Network::Stagenet | Network::DevnetAmplifier => {
                "testnet".to_string()
            }
        })
}

fn wei_to_eth(wei: alloy::primitives::U256) -> f64 {
    let divisor = alloy::primitives::U256::from(1_000_000_000_000_000_000u64); // 10^18
    let whole = wei / divisor;
    let remainder = wei % divisor;
    let whole_f64: f64 = whole.to::<u128>() as f64;
    let remainder_f64: f64 = remainder.to::<u128>() as f64 / 1e18;
    whole_f64 + remainder_f64
}

fn render_table(rows: &[BalanceRow]) {
    let mut table = Table::new();
    table.load_preset(comfy_table::presets::UTF8_FULL);
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec![
        header_cell("Chain"),
        header_cell("Type"),
        header_cell("Address"),
        header_cell("Balance"),
        header_cell("Min"),
        header_cell("Status"),
        header_cell("Note"),
    ]);
    for row in rows {
        let (status_label, status_color) = if row.is_underfunded() {
            ("LOW", Color::Red)
        } else {
            ("ok", Color::Green)
        };
        table.add_row(vec![
            Cell::new(&row.chain_key).fg(Color::Cyan),
            Cell::new(row.kind.label()),
            Cell::new(row.address.as_deref().unwrap_or("-")),
            Cell::new(format_balance(row.balance, &row.token_symbol)),
            Cell::new(format!("{:.4} {}", row.threshold, row.token_symbol)),
            Cell::new(status_label).fg(status_color),
            Cell::new(row.note.as_str()).fg(Color::DarkGrey),
        ]);
    }
    println!();
    println!("{table}");
}

fn header_cell(label: &str) -> Cell {
    Cell::new(label)
        .fg(Color::Cyan)
        .add_attribute(Attribute::Bold)
}

fn format_balance(balance: Option<f64>, symbol: &str) -> String {
    match balance {
        Some(b) => format!("{b:.4} {symbol}"),
        None => "-".to_string(),
    }
}

fn format_underfunded(row: &BalanceRow) -> String {
    let address = row.address.as_deref().unwrap_or("?");
    match row.balance {
        Some(b) => format!(
            "{} ({}): {} has {:.4} {} (need >= {} {})",
            row.chain_key,
            row.kind.label(),
            address,
            b,
            row.token_symbol,
            row.threshold,
            row.token_symbol
        ),
        None => format!(
            "{} ({}): {} — {}",
            row.chain_key,
            row.kind.label(),
            address,
            if row.note.is_empty() {
                "balance unknown"
            } else {
                &row.note
            },
        ),
    }
}

fn shorten_error(error: &str) -> String {
    const LIMIT: usize = 80;
    if error.chars().count() <= LIMIT {
        return error.to_string();
    }
    let truncated: String = error.chars().take(LIMIT).collect();
    format!("{truncated}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mainnet_uses_stellar_top_level_key() {
        let targets = chain_targets(Network::Mainnet);
        let stellar = targets
            .iter()
            .find(|t| t.kind == ChainKind::Stellar)
            .unwrap();
        assert_eq!(stellar.chain_key, "stellar");
    }

    #[test]
    fn testnet_uses_quarterly_stellar_key() {
        for net in [
            Network::Testnet,
            Network::Stagenet,
            Network::DevnetAmplifier,
        ] {
            let targets = chain_targets(net);
            let stellar = targets
                .iter()
                .find(|t| t.kind == ChainKind::Stellar)
                .unwrap();
            assert_eq!(stellar.chain_key, "stellar-2026-q1-2", "for {net:?}");
        }
    }

    #[test]
    fn thresholds_match_removed_js_script() {
        // Map: chain_key -> expected threshold (mirrors
        // `axelar-contract-deployments/scripts/check-wallet-balances.js` THRESHOLDS).
        // Solana (0.3) is axe-specific — not in the original JS.
        let expected: &[(&str, f64)] = &[
            ("hyperliquid", 0.01),
            ("xrpl-evm", 2.0),
            ("xrpl", 3.0),
            ("stellar", 0.5),
            ("sui", 0.05),
            ("solana", 0.3),
        ];
        let targets = chain_targets(Network::Mainnet);
        for (key, want) in expected {
            let got = targets
                .iter()
                .find(|t| t.chain_key == *key)
                .unwrap_or_else(|| panic!("no target for {key}"));
            assert!(
                (got.threshold_units - *want).abs() < f64::EPSILON,
                "{key}: got {} want {want}",
                got.threshold_units
            );
        }
    }

    #[test]
    fn underfunded_when_balance_below_threshold() {
        let row = BalanceRow {
            chain_key: "hyperliquid".to_string(),
            kind: ChainKind::Evm,
            address: Some("0xabc".to_string()),
            balance: Some(0.005),
            threshold: 0.01,
            token_symbol: "ETH".to_string(),
            note: String::new(),
        };
        assert!(row.is_underfunded());
    }

    #[test]
    fn ok_when_balance_above_threshold() {
        let row = BalanceRow {
            chain_key: "hyperliquid".to_string(),
            kind: ChainKind::Evm,
            address: Some("0xabc".to_string()),
            balance: Some(1.0),
            threshold: 0.01,
            token_symbol: "ETH".to_string(),
            note: String::new(),
        };
        assert!(!row.is_underfunded());
    }

    #[test]
    fn underfunded_when_balance_is_none() {
        // Probe error => no balance => treat as underfunded (better safe than sorry).
        let row = BalanceRow {
            chain_key: "hyperliquid".to_string(),
            kind: ChainKind::Evm,
            address: None,
            balance: None,
            threshold: 0.01,
            token_symbol: "ETH".to_string(),
            note: "RPC unreachable".to_string(),
        };
        assert!(row.is_underfunded());
    }
}
