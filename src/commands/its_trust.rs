//! `axe its-trust`: the owner's half of the trust matrix: make one chain's
//! ITS trust every live chain on the ITS Hub. Reads the hub registry, reports
//! what the ITS already trusts, and with `--apply` sends one owner `multicall`
//! of the missing setters after showing it in full and asking for `y`.
//!
//! The other half (every other chain trusting this one) runs through their
//! ASGs: `axe propose --op set-trusted --its-chain <chain> --chains all`.

use std::str::FromStr;

use alloy::primitives::{Address, Bytes};
use alloy::providers::Provider;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolCall;
use eyre::{Result, WrapErr};

use crate::commands::decode;
use crate::config::{ChainContract, ChainsConfig};
use crate::config_source;
use crate::evm::its_trust::{self, TrustApi};
use crate::evm::{InterchainTokenService, broadcast_and_log, connect_evm, connect_evm_signed};
use crate::its_hub::{self, RegisteredChain};
use crate::types::Network;
use crate::ui;

const OWNER_KEY_ENV: &str = "ITS_OWNER_PRIVATE_KEY";

/// The ITS whose trust list is managed.
struct Edge {
    axelar_id: String,
    rpc: String,
    its: Address,
}

/// A live hub chain and whether the edge ITS trusts it.
struct Peer {
    chain: String,
    trusted: bool,
}

struct Excluded {
    chain: String,
    reason: &'static str,
}

pub async fn run(network: Network, chain: &str, skip: &[String], apply: bool) -> Result<()> {
    if apply {
        ui::require_interactive("axe its-trust --apply")?;
    }
    let config_path = config_source::resolve(network, None).await?.into_path();
    let config = ChainsConfig::load(&config_path).await?;
    let edge = resolve_edge(&config, chain)?;
    let provider = connect_evm(std::slice::from_ref(&edge.rpc))?;
    let api = its_trust::probe(&provider, edge.its, &edge.axelar_id).await?;
    ui::section(&format!("ITS trust: {} on {network}", edge.axelar_id));
    ui::address("ITS", &edge.its.to_string());
    ui::kv("trust API", api.label());

    let registry = its_hub::registered_chains(&config).await?;
    let (wanted, excluded) = select_peers(&registry, network, &edge.axelar_id, skip);
    let peers = read_trust(&provider, edge.its, api, &wanted).await?;
    report(&peers, &excluded);
    let missing: Vec<String> = untrusted(&peers);
    if missing.is_empty() {
        ui::success(&format!(
            "{} trusts all {} live hub chains",
            edge.axelar_id,
            peers.len()
        ));
        return Ok(());
    }
    if !apply {
        ui::info(&format!(
            "{} chain(s) not trusted yet; rerun with --apply ({OWNER_KEY_ENV} must be the ITS owner)",
            missing.len()
        ));
        return Ok(());
    }

    let owner = InterchainTokenService::new(edge.its, &provider)
        .owner()
        .call()
        .await
        .wrap_err("cannot read ITS owner")?;
    apply_trust(&edge, owner, api, &missing).await?;
    let still_missing = untrusted(&read_trust(&provider, edge.its, api, &missing).await?);
    eyre::ensure!(
        still_missing.is_empty(),
        "multicall confirmed but {} still untrusted: {}",
        edge.axelar_id,
        still_missing.join(", ")
    );
    ui::success(&format!(
        "{} now trusts all {} live hub chains",
        edge.axelar_id,
        peers.len()
    ));
    Ok(())
}

/// The chain's ITS and RPC from the config, by key or axelar ID.
fn resolve_edge(config: &ChainsConfig, chain: &str) -> Result<Edge> {
    let (key, cfg) = config
        .chains
        .iter()
        .find(|(key, cfg)| {
            key.eq_ignore_ascii_case(chain) || cfg.axelar_id_or(key).eq_ignore_ascii_case(chain)
        })
        .ok_or_else(|| eyre::eyre!("'{chain}' is not in the chains config"))?;
    eyre::ensure!(
        cfg.chain_type.as_deref() == Some("evm"),
        "'{chain}' is not an EVM chain; axe its-trust manages EVM ITS only"
    );
    let axelar_id = cfg.axelar_id_or(key);
    let rpc = cfg
        .rpc
        .clone()
        .ok_or_else(|| eyre::eyre!("no rpc for '{chain}' in config"))?;
    let its = cfg
        .contract_address(ChainContract::InterchainTokenService, &axelar_id)
        .wrap_err("no InterchainTokenService in config")?;
    let its = Address::from_str(its).wrap_err("invalid ITS address in config")?;
    Ok(Edge {
        axelar_id,
        rpc,
        its,
    })
}

/// The hub chains the edge should trust, in the hub's spelling, and the ones
/// left out with the reason. Pure.
fn select_peers(
    registry: &[RegisteredChain],
    network: Network,
    own_axelar_id: &str,
    skip: &[String],
) -> (Vec<String>, Vec<Excluded>) {
    let mut wanted = Vec::new();
    let mut excluded = Vec::new();
    for entry in registry {
        let reason = if entry.frozen {
            Some("frozen on the ITS Hub")
        } else if entry.chain.eq_ignore_ascii_case(own_axelar_id) {
            Some("itself")
        } else if its_hub::is_deprecated(network, &entry.chain) {
            Some("deprecated")
        } else if skip.iter().any(|s| s.eq_ignore_ascii_case(&entry.chain)) {
            Some("skipped (--skip)")
        } else {
            None
        };
        match reason {
            Some(reason) => excluded.push(Excluded {
                chain: entry.chain.clone(),
                reason,
            }),
            None => wanted.push(entry.chain.clone()),
        }
    }
    (wanted, excluded)
}

async fn read_trust<P: Provider>(
    provider: &P,
    its: Address,
    api: TrustApi,
    chains: &[String],
) -> Result<Vec<Peer>> {
    let mut peers = Vec::with_capacity(chains.len());
    for chain in chains {
        let trusted = its_trust::is_trusted(provider, its, api, chain)
            .await
            .wrap_err_with(|| format!("cannot read whether '{chain}' is trusted"))?;
        peers.push(Peer {
            chain: chain.clone(),
            trusted,
        });
    }
    Ok(peers)
}

fn untrusted(peers: &[Peer]) -> Vec<String> {
    peers
        .iter()
        .filter(|peer| !peer.trusted)
        .map(|peer| peer.chain.clone())
        .collect()
}

fn report(peers: &[Peer], excluded: &[Excluded]) {
    ui::section("hub chains");
    for peer in peers {
        ui::kv(
            &peer.chain,
            if peer.trusted {
                "trusted"
            } else {
                "NOT trusted"
            },
        );
    }
    for entry in excluded {
        ui::kv(&format!("skip {}", entry.chain), entry.reason);
    }
}

/// One owner-signed `multicall` of a setter per missing chain, sent only
/// after the full calldata was shown and approved.
async fn apply_trust(edge: &Edge, owner: Address, api: TrustApi, missing: &[String]) -> Result<()> {
    let signer = owner_signer(owner)?;
    let provider = connect_evm_signed(std::slice::from_ref(&edge.rpc), signer)?;
    let balance = provider.get_balance(owner).await?;
    eyre::ensure!(
        !balance.is_zero(),
        "ITS owner {owner} has zero balance on {}; fund it for gas",
        edge.axelar_id
    );
    let calls = encode_multicall(missing, api);
    let input = InterchainTokenService::multicallCall {
        data: calls.clone(),
    }
    .abi_encode();
    approve(edge, owner, missing, &input).await?;
    let pending = InterchainTokenService::new(edge.its, &provider)
        .multicall(calls)
        .send()
        .await?;
    let receipt = broadcast_and_log(pending, "ITS.multicall tx").await?;
    eyre::ensure!(receipt.status(), "ITS.multicall reverted");
    Ok(())
}

fn encode_multicall(chains: &[String], api: TrustApi) -> Vec<Bytes> {
    chains
        .iter()
        .map(|chain| its_trust::encode_set(chain, api))
        .collect()
}

/// The ITS owner's key from the environment, refused unless it derives `owner`.
fn owner_signer(owner: Address) -> Result<PrivateKeySigner> {
    let key = std::env::var(OWNER_KEY_ENV)
        .map_err(|_| eyre::eyre!("set {OWNER_KEY_ENV} to the ITS owner {owner}"))?;
    let signer: PrivateKeySigner = key
        .trim()
        .parse()
        .map_err(|_| eyre::eyre!("{OWNER_KEY_ENV} is not a valid private key"))?;
    eyre::ensure!(
        signer.address() == owner,
        "{OWNER_KEY_ENV} derives {}, not the ITS owner {owner}",
        signer.address()
    );
    Ok(signer)
}

/// Show the transaction in full and wait for `y`.
async fn approve(edge: &Edge, owner: Address, missing: &[String], input: &[u8]) -> Result<()> {
    ui::section("ITS.multicall (trust chains)");
    ui::kv("chain", &edge.axelar_id);
    ui::address("signer (ITS owner)", &owner.to_string());
    ui::address("to (ITS)", &edge.its.to_string());
    ui::kv("chains", &missing.join(", "));
    let calldata_hex = format!("0x{}", alloy::hex::encode(input));
    ui::kv("input", &calldata_hex);
    // Pretty-print the nested setters with the `axe decode` decoder.
    let _ = decode::run(&calldata_hex);
    eyre::ensure!(
        ui::confirm("Send this transaction?").await,
        "declined; nothing was sent"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evm::its_trust::{setTrustedAddressCall, setTrustedChainCall};

    fn registered(chain: &str, frozen: bool) -> RegisteredChain {
        RegisteredChain {
            chain: chain.to_string(),
            its_edge_contract: String::new(),
            frozen,
        }
    }

    #[test]
    fn peers_keep_the_hub_spelling_and_explain_every_exclusion() {
        let registry = [
            registered("Ethereum", false),
            registered("arc", false),
            registered("berachain", true),
            registered("Moonbeam", false),
            registered("solana", false),
            registered("xrpl", false),
        ];
        let (wanted, excluded) =
            select_peers(&registry, Network::Mainnet, "arc", &["XRPL".to_string()]);
        assert_eq!(wanted, ["Ethereum", "solana"]);
        let reasons: Vec<(&str, &str)> = excluded
            .iter()
            .map(|e| (e.chain.as_str(), e.reason))
            .collect();
        assert_eq!(
            reasons,
            [
                ("arc", "itself"),
                ("berachain", "frozen on the ITS Hub"),
                ("Moonbeam", "deprecated"),
                ("xrpl", "skipped (--skip)"),
            ]
        );
    }

    #[test]
    fn deprecation_only_applies_on_mainnet() {
        let registry = [registered("Moonbeam", false)];
        let (wanted, _) = select_peers(&registry, Network::Testnet, "arc", &[]);
        assert_eq!(wanted, ["Moonbeam"]);
    }

    #[test]
    fn multicall_holds_one_setter_per_chain_for_the_probed_api() {
        let chains = ["Ethereum".to_string(), "solana".to_string()];
        let new = encode_multicall(&chains, TrustApi::TrustedChain);
        assert_eq!(new.len(), 2);
        let first = setTrustedChainCall::abi_decode(&new[0]).unwrap();
        assert_eq!(first.chainName, "Ethereum");
        let old = encode_multicall(&chains, TrustApi::TrustedAddress);
        let second = setTrustedAddressCall::abi_decode(&old[1]).unwrap();
        assert_eq!(
            (second.chain.as_str(), second.address_.as_str()),
            ("solana", "hub")
        );
    }
}
