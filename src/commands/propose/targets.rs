//! Which edge chains a proposal targets, and why the rest are left out.
//!
//! `--chains all` starts from the ITS Hub's own registry, so the list is what
//! the hub routes to today, not what a config file remembers. Each candidate
//! is then narrowed to live EVM chains whose ITS the ASG owns, and anything
//! that cannot be verified stops the run: a Cosmos gov proposal executes all
//! of its messages or none, so one bad chain would void every other call.

use std::str::FromStr;

use alloy::primitives::Address;
use alloy::providers::ProviderBuilder;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use serde_json::json;

use crate::config::{AxelarGlobalContract, ChainContract, ChainsConfig};
use crate::cosmos::lcd_cosmwasm_smart_query;
use crate::evm::InterchainTokenService;
use crate::its_hub::{self, RegisteredChain};
use crate::types::Network;
use crate::{http, ui};

use super::helpers;
use super::types::{EdgeRoute, ProposeArgs, ResolvedConfig};

/// A hub chain that passed every static filter and still needs resolving.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// JSON key in the chains config.
    pub key: String,
    /// Edge contract the hub routes to. The config must agree.
    pub its_edge: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exclusion {
    pub chain: String,
    pub reason: String,
}

/// A resolved edge chain the proposal will address.
pub struct Target {
    pub chain: String,
    pub cfg: ResolvedConfig,
}

/// Resolve the chain(s) named on the command line into verified targets.
pub async fn select(args: &ProposeArgs, config: &ChainsConfig) -> Result<Vec<Target>> {
    match (&args.chain, args.chains.as_slice()) {
        (Some(chain), []) => Ok(vec![Target {
            chain: chain.clone(),
            cfg: helpers::resolve(args.network, config, chain)?,
        }]),
        (None, [all]) if all == "all" => select_all(args, config).await,
        (None, keys) if !keys.is_empty() => {
            let candidates = keys
                .iter()
                .map(|key| Candidate {
                    key: key.clone(),
                    its_edge: String::new(),
                })
                .collect();
            resolve_all(args.network, config, candidates, &[]).await
        }
        _ => Err(eyre::eyre!("name one chain, or pass --chains all|a,b,c")),
    }
}

async fn select_all(args: &ProposeArgs, config: &ChainsConfig) -> Result<Vec<Target>> {
    let trusted = args.its_chain.as_deref().unwrap_or_default();
    let hub = its_hub::registered_chains(config).await?;
    let (candidates, excluded) = classify(&hub, config, args.network, &args.skip, trusted);
    eyre::ensure!(
        !candidates.is_empty(),
        "no eligible edge chains on the ITS Hub after exclusions"
    );
    resolve_all(args.network, config, candidates, &excluded).await
}

/// Static filters, in the order a reviewer would ask about them. Pure.
pub fn classify(
    hub: &[RegisteredChain],
    config: &ChainsConfig,
    network: Network,
    skip: &[String],
    trusted_chain: &str,
) -> (Vec<Candidate>, Vec<Exclusion>) {
    let mut candidates = Vec::new();
    let mut excluded = Vec::new();
    for chain in hub {
        let exclude = |reason: &str| Exclusion {
            chain: chain.chain.clone(),
            reason: reason.to_string(),
        };
        if chain.frozen {
            excluded.push(exclude("frozen on the ITS Hub"));
            continue;
        }
        if chain.chain.eq_ignore_ascii_case(trusted_chain) {
            excluded.push(exclude("the chain being trusted"));
            continue;
        }
        let Some((key, chain_cfg)) = config
            .chains
            .iter()
            .find(|(key, cfg)| cfg.axelar_id_or(key).eq_ignore_ascii_case(&chain.chain))
        else {
            excluded.push(exclude("not in the chains config"));
            continue;
        };
        if chain_cfg.chain_type.as_deref() != Some("evm") {
            excluded.push(exclude("not an EVM chain"));
            continue;
        }
        if its_hub::is_deprecated(network, key) {
            excluded.push(exclude("deprecated"));
            continue;
        }
        if skip.iter().any(|skipped| skipped.eq_ignore_ascii_case(key)) {
            excluded.push(exclude("skipped (--skip)"));
            continue;
        }
        candidates.push(Candidate {
            key: key.clone(),
            its_edge: chain.its_edge_contract.clone(),
        });
    }
    (candidates, excluded)
}

/// Resolve every candidate or stop with all of the failures at once.
async fn resolve_all(
    network: Network,
    config: &ChainsConfig,
    candidates: Vec<Candidate>,
    excluded: &[Exclusion],
) -> Result<Vec<Target>> {
    ui::section("edge chains");
    let mut targets = Vec::new();
    let mut failures = Vec::new();
    for candidate in candidates {
        match resolve_target(network, config, &candidate).await {
            Ok(target) => {
                ui::kv(
                    &target.chain,
                    &format!(
                        "{} via {}; ASG {}",
                        target.cfg.edge_axelar_id,
                        target.cfg.route().label(),
                        target.cfg.asg_address
                    ),
                );
                targets.push(target);
            }
            Err(error) => failures.push(format!("{}: {error:#}", candidate.key)),
        }
    }
    for exclusion in excluded {
        ui::kv(&format!("skip {}", exclusion.chain), &exclusion.reason);
    }
    eyre::ensure!(
        failures.is_empty(),
        "cannot verify every edge chain; fix the config or leave them out with --skip:\n  {}",
        failures.join("\n  ")
    );
    Ok(targets)
}

/// One chain's config, with its ASG taken from the config or, failing that,
/// from the ITS's on-chain owner. `verify_asg` later proves that address is a
/// real ASG wired to the gov module.
async fn resolve_target(
    network: Network,
    config: &ChainsConfig,
    candidate: &Candidate,
) -> Result<Target> {
    let key = candidate.key.as_str();
    let chain_cfg = config
        .chains
        .get(key)
        .ok_or_else(|| eyre::eyre!("not in the chains config"))?;
    let edge_id = chain_cfg.axelar_id_or(key);
    let its = chain_cfg
        .contract_address(ChainContract::InterchainTokenService, &edge_id)
        .wrap_err("no InterchainTokenService in config")?;
    if !candidate.its_edge.is_empty() && !its.eq_ignore_ascii_case(&candidate.its_edge) {
        return Err(eyre::eyre!(
            "ITS {its} in config differs from the ITS Hub edge {}",
            candidate.its_edge
        ));
    }
    let asg = match chain_cfg.contract_address(ChainContract::AxelarServiceGovernance, &edge_id) {
        Ok(asg) => asg.to_string(),
        Err(_) => {
            let rpc = chain_cfg
                .rpc
                .as_deref()
                .ok_or_else(|| eyre::eyre!("no rpc in config"))?;
            its_owner(rpc, its).await?.to_string()
        }
    };
    let cfg = helpers::resolve_with_asg(network, config, key, Some(&asg))?;
    Ok(Target {
        chain: key.to_string(),
        cfg,
    })
}

async fn its_owner(rpc: &str, its: &str) -> Result<Address> {
    let provider =
        ProviderBuilder::new().connect_http(rpc.parse().wrap_err("invalid rpc url in config")?);
    let its = Address::from_str(its).wrap_err("invalid ITS address in config")?;
    InterchainTokenService::new(its, &provider)
        .owner()
        .call()
        .await
        .wrap_err("cannot read ITS owner (the ASG fallback)")
}

#[derive(Deserialize)]
struct NexusChains {
    chains: Vec<String>,
}

/// Prove every destination name routes before the deposit is spent. Amplifier
/// chains must be on the Router, consensus chains on the nexus registry, whose
/// legacy names keep their capitals (`Ethereum`, `Avalanche`).
pub async fn check_route_names(config: &ChainsConfig, targets: &[Target]) -> Result<()> {
    let (lcd, _, _, _) = config.axelar.cosmos_tx_params()?;
    let lcd = lcd.trim_end_matches('/');
    let router = config
        .axelar
        .global_contract_address(AxelarGlobalContract::Router)?;
    let nexus: NexusChains = http::get_json(format!("{lcd}/axelar/nexus/v1beta1/chains").parse()?)
        .await
        .wrap_err("cannot list nexus chains")?;
    for target in targets {
        let edge = &target.cfg.edge_axelar_id;
        match target.cfg.route() {
            EdgeRoute::Amplifier => {
                let info = lcd_cosmwasm_smart_query(lcd, router, &json!({"chain_info": edge}))
                    .await
                    .wrap_err_with(|| format!("'{edge}' is not a chain on the Router"))?;
                eyre::ensure!(
                    info["name"].as_str() == Some(edge),
                    "Router knows '{edge}' under a different name: {}",
                    info["name"]
                );
            }
            EdgeRoute::Consensus => eyre::ensure!(
                nexus.chains.iter().any(|chain| chain == edge),
                "'{edge}' is not a chain on Axelar's nexus registry (names are case-sensitive)"
            ),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ChainsConfig {
        ChainsConfig::from_json_str(
            r#"{
              "chains": {
                "ethereum": {"axelarId": "Ethereum", "chainType": "evm"},
                "monad": {"axelarId": "monad", "chainType": "evm"},
                "moonbeam": {"axelarId": "Moonbeam", "chainType": "evm"},
                "solana": {"axelarId": "solana", "chainType": "svm"},
                "arc": {"axelarId": "arc", "chainType": "evm"}
              },
              "axelar": {}
            }"#,
        )
        .unwrap()
    }

    fn hub(chain: &str, frozen: bool) -> RegisteredChain {
        RegisteredChain {
            chain: chain.into(),
            its_edge_contract: format!("0x{chain}"),
            frozen,
        }
    }

    #[test]
    fn classify_keeps_live_evm_asg_chains_and_explains_the_rest() {
        let registry = vec![
            hub("Ethereum", false),
            hub("monad", false),
            hub("Moonbeam", false),
            hub("solana", false),
            hub("plume", true),
            hub("arc", false),
            hub("sui", false),
        ];
        let (candidates, excluded) = classify(
            &registry,
            &config(),
            Network::Mainnet,
            &["monad".to_string()],
            "arc",
        );
        assert_eq!(
            candidates,
            vec![Candidate {
                key: "ethereum".into(),
                its_edge: "0xEthereum".into()
            }]
        );
        let reasons: Vec<(String, String)> =
            excluded.into_iter().map(|e| (e.chain, e.reason)).collect();
        assert_eq!(
            reasons,
            vec![
                ("monad".to_string(), "skipped (--skip)".to_string()),
                ("Moonbeam".to_string(), "deprecated".to_string()),
                ("solana".to_string(), "not an EVM chain".to_string()),
                ("plume".to_string(), "frozen on the ITS Hub".to_string()),
                ("arc".to_string(), "the chain being trusted".to_string()),
                ("sui".to_string(), "not in the chains config".to_string()),
            ]
        );
    }

    #[test]
    fn deprecation_list_is_mainnet_only() {
        let registry = vec![hub("Moonbeam", false)];
        let (candidates, _) = classify(&registry, &config(), Network::Testnet, &[], "arc");
        assert_eq!(candidates.len(), 1);
    }
}
