//! Mainnet's established EVM addresses. Never silently substitute a different deployment.
use alloy::{
    primitives::{Address, B256, address, b256, keccak256},
    providers::Provider,
    signers::local::PrivateKeySigner,
};
use eyre::Result;

use super::{
    inputs, storage,
    types::{Journal, Transaction},
};
use crate::{state::State, types::Network, ui};

mod predictions;
#[cfg(test)]
mod tests;

pub const DEPLOYER: Address = address!("6f24A47Fc8AE5441Eb47EFfC3665e70e69Ac3F05");
pub const GATEWAY_DEPLOYER: Address = address!("B8Cd93C83A974649D76B1c19f311f639e62272BC");
pub const CONST_DEPLOYER: Address = address!("98B2920D53612483F91F12Ed7754E51b4A77919e");
pub const CONST_DEPLOYER_SIGNER: Address = address!("E86375704CDb8491a5Ed82D90DceCE02Ee0ac25F");
pub const CREATE3_DEPLOYER: Address = address!("6513Aedb4D1593BA12e50644401D976aebDc90d8");
pub const GATEWAY: Address = address!("e432150cce91c13a887f7D836923d5597adD8E31");
pub const OPERATORS: Address = address!("7DdB2d76b80B0AA19bDEa48EB1301182F4CeefbC");
pub const GAS_SERVICE: Address = address!("2d5d7d31F671F86C782533cc367F14109a082712");
pub const ITS: Address = address!("B5FB4BE02232B1bBA4dC8f81dc24C26980dE9e3C");
pub const ITS_FACTORY: Address = address!("83a93500d23Fbc3e82B410aD07A6a9F7A0670D66");
pub const ITS_IMPLEMENTATION: Address = address!("4Aca663c242D31F85b77bf27d5D7dA4cF69fa7bd");
pub const ITS_FACTORY_IMPLEMENTATION: Address =
    address!("6977477Bd4C053E1BfD0B6F287060c2A4E5E1aA1");
const CONST_HASH: B256 = b256!("8fda47a596dfba923270da84e0c32a2d0312f1c03389f83e16f2b5a35ed37fbe");
const CREATE3_HASH: B256 =
    b256!("f0ad66defbe082df243d4d274e626f557f97579c5c9e19f33d8093d6160808b7");

pub fn expected(name: &str) -> Option<Address> {
    match name {
        "ConstAddressDeployer" => Some(CONST_DEPLOYER),
        "Create3Deployer" => Some(CREATE3_DEPLOYER),
        "AxelarGateway" => Some(GATEWAY),
        "Operators" => Some(OPERATORS),
        "AxelarGasService" => Some(GAS_SERVICE),
        "InterchainTokenService" => Some(ITS),
        "InterchainTokenFactory" => Some(ITS_FACTORY),
        "InterchainTokenServiceImpl" => Some(ITS_IMPLEMENTATION),
        "InterchainTokenFactoryImpl" => Some(ITS_FACTORY_IMPLEMENTATION),
        _ => None,
    }
}

pub fn check_address(network: Network, name: &str, actual: Address) -> Result<()> {
    if network == Network::Mainnet
        && let Some(expected) = expected(name)
    {
        eyre::ensure!(
            actual == expected,
            "{name}: mainnet requires {expected}, predicted/found {actual}. Restore the canonical signer, salt, factory and artifacts; no alternative address is allowed"
        );
    }
    Ok(())
}

pub fn salt<'a>(network: Network, name: &'a str, configured: &'a str) -> &'a str {
    if network == Network::Mainnet {
        name
    } else {
        configured
    }
}

pub async fn factory<P: Provider>(provider: &P, name: &str) -> Result<Address> {
    let (address, hash) = match name {
        "ConstAddressDeployer" => (CONST_DEPLOYER, CONST_HASH),
        "Create3Deployer" => (CREATE3_DEPLOYER, CREATE3_HASH),
        _ => {
            eyre::bail!("unknown canonical factory {name}");
        }
    };
    let code = provider.get_code_at(address).await?;
    eyre::ensure!(
        !code.is_empty(),
        "{name} is missing at {address}. Bootstrap the canonical factory using the reference deployment procedure before starting axe. Axe will not replace it with a different address"
    );
    eyre::ensure!(
        keccak256(code) == hash,
        "{name} at {address} has unexpected runtime code; stop and reconcile the factory deployment"
    );
    Ok(address)
}

pub async fn validate<P: Provider>(state: &State, provider: &P) -> Result<()> {
    if state.env != Network::Mainnet {
        return Ok(());
    }
    validate_signers(state)?;
    validate_salts(state)?;
    factory(provider, "ConstAddressDeployer").await?;
    if !provider.get_code_at(CREATE3_DEPLOYER).await?.is_empty() {
        factory(provider, "Create3Deployer").await?;
    }
    predictions::validate(&inputs::root(state)?).await?;
    reject_unrecorded_deployments(state, provider).await?;
    gateway_nonce(state, provider).await?;
    ui::section("Canonical mainnet addresses");
    for name in [
        "ConstAddressDeployer",
        "Create3Deployer",
        "AxelarGateway",
        "Operators",
        "AxelarGasService",
        "InterchainTokenService",
        "InterchainTokenFactory",
        "InterchainTokenServiceImpl",
        "InterchainTokenFactoryImpl",
    ] {
        if let Some(address) = expected(name) {
            ui::address(name, &address.to_string());
        }
    }
    ui::info(
        "Mainnet uses the established factory addresses and contract-specific EVM salts. SALT remains the Cosmos deployment salt. Configured final owners are unchanged.",
    );
    Ok(())
}

async fn reject_unrecorded_deployments<P: Provider>(state: &State, provider: &P) -> Result<()> {
    let path = storage::directory(state)?.join("journal.json");
    let journal: Option<Journal> = if path.exists() {
        Some(serde_json::from_slice(&tokio::fs::read(path).await?)?)
    } else {
        None
    };
    for (action, name, address) in [
        ("AxelarGateway/gateway proxy", "AxelarGateway", GATEWAY),
        ("Operators/Operators", "Operators", OPERATORS),
        (
            "AxelarGasService/gas proxy",
            "AxelarGasService",
            GAS_SERVICE,
        ),
        (
            "DeployInterchainTokenService/InterchainTokenServiceProxy",
            "InterchainTokenService",
            ITS,
        ),
        (
            "DeployInterchainTokenService/InterchainTokenFactoryProxy",
            "InterchainTokenFactory",
            ITS_FACTORY,
        ),
        (
            "DeployInterchainTokenService/InterchainTokenServiceImpl",
            "InterchainTokenServiceImpl",
            ITS_IMPLEMENTATION,
        ),
        (
            "DeployInterchainTokenService/InterchainTokenFactoryImpl",
            "InterchainTokenFactoryImpl",
            ITS_FACTORY_IMPLEMENTATION,
        ),
    ] {
        if !journal
            .as_ref()
            .is_some_and(|j| j.actions.contains_key(action))
        {
            eyre::ensure!(
                provider.get_code_at(address).await?.is_empty(),
                "{name} already exists at {address} without a deployment journal action. Reconcile the existing deployment before starting; axe will not overwrite or adopt it"
            );
        }
    }
    Ok(())
}

fn validate_signers(state: &State) -> Result<()> {
    for (name, key, expected) in [
        (
            "DEPLOYER_PRIVATE_KEY",
            state.deployer_private_key.as_deref(),
            DEPLOYER,
        ),
        (
            "GATEWAY_DEPLOYER_PRIVATE_KEY",
            state.gateway_deployer_private_key.as_deref(),
            GATEWAY_DEPLOYER,
        ),
        (
            "GAS_SERVICE_DEPLOYER_PRIVATE_KEY",
            state.gas_service_deployer_private_key.as_deref(),
            DEPLOYER,
        ),
        (
            "ITS_DEPLOYER_PRIVATE_KEY",
            state.its_deployer_private_key.as_deref(),
            DEPLOYER,
        ),
    ] {
        let signer: PrivateKeySigner = key.ok_or_else(|| eyre::eyre!("missing {name}"))?.parse()?;
        eyre::ensure!(
            signer.address() == expected,
            "{name} must sign as {expected} for canonical mainnet addresses; supplied signer is {}",
            signer.address()
        );
    }
    Ok(())
}

fn validate_salts(state: &State) -> Result<()> {
    eyre::ensure!(
        state.its_salt.as_deref() == Some("v2.2.0")
            && state.its_proxy_salt.as_deref() == Some("v1.0.0"),
        "mainnet requires ITS_SALT=v2.2.0 and ITS_PROXY_SALT=v1.0.0 for this release"
    );
    if let Some(address) = state.predicted_gateway_address {
        check_address(state.env, "AxelarGateway", address)?;
    }
    Ok(())
}

async fn gateway_nonce<P: Provider>(state: &State, provider: &P) -> Result<()> {
    let path = storage::directory(state)?.join("journal.json");
    if path.exists() {
        let journal: Journal = serde_json::from_slice(&tokio::fs::read(path).await?)?;
        if let Some(action) = journal.actions.get("AxelarGateway/gateway implementation") {
            eyre::ensure!(
                matches!(action, Transaction::Evm { sender, nonce: 2, .. } if *sender == GATEWAY_DEPLOYER),
                "gateway journal does not match canonical mainnet nonce 2"
            );
            return Ok(());
        }
    }
    let latest = provider.get_transaction_count(GATEWAY_DEPLOYER).await?;
    let pending = provider
        .get_transaction_count(GATEWAY_DEPLOYER)
        .pending()
        .await?;
    validate_gateway_nonce(latest, pending)?;
    eyre::ensure!(
        provider.get_code_at(GATEWAY).await?.is_empty(),
        "canonical gateway already exists; a new deployment cannot adopt it"
    );
    Ok(())
}

fn validate_gateway_nonce(latest: u64, pending: u64) -> Result<()> {
    eyre::ensure!(
        latest == 2 && pending == 2,
        "canonical mainnet gateway requires confirmed and pending nonce 2 for {GATEWAY_DEPLOYER}; found {latest}/{pending}. Implementation uses nonce 2, proxy uses nonce 3. Prepare this account outside axe before starting; do not send from it during deployment. Axe does not burn nonces automatically"
    );
    Ok(())
}
