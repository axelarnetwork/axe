use std::collections::BTreeMap;
use std::str::FromStr;

use eyre::Result;

use super::{plan, types::Plan};
use crate::cosmos::derive_axelar_wallet;
use crate::types::Network;

#[cfg(test)]
mod tests;

const FIELDS: [&str; 13] = [
    "CHAIN_ID",
    "AXELAR_CHAIN_ID",
    "GATEWAY_OWNER",
    "OPERATORS_OWNER",
    "GAS_SERVICE_OWNER",
    "ITS_OWNER",
    "FACTORY_OWNER",
    "GATEWAY_OPERATOR",
    "EVM_GAS_BUDGET",
    "COSMOS_FEE_BUDGET",
    "REWARD_AMOUNT",
    "BLOCK_EXPIRY",
    "CONFIRMATION_HEIGHT",
];

/// Preserve saved JSON-plan runs, but never silently ignore a partial .env plan.
pub(super) fn load(
    network: Network,
    saved: Option<&Plan>,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Plan> {
    let values: BTreeMap<_, _> = FIELDS
        .iter()
        .filter_map(|name| lookup(name).map(|value| (*name, value.trim().to_owned())))
        .collect();
    if let Some(saved) = saved
        && values.keys().all(|name| *name == "CHAIN_ID")
    {
        if let Some(chain_id) = values.get("CHAIN_ID") {
            eyre::ensure!(
                parse::<u64>("CHAIN_ID", chain_id)? == saved.evm_chain_id,
                "CHAIN_ID differs from the saved deployment"
            );
        }
        return Ok(saved.clone());
    }
    let missing: Vec<_> = FIELDS
        .iter()
        .filter(|name| values.get(**name).is_none_or(String::is_empty))
        .copied()
        .collect();
    eyre::ensure!(
        missing.is_empty(),
        "missing deployment variables in .env: {}",
        missing.join(", ")
    );
    let value = |name| values.get(name).map(String::as_str).unwrap_or_default();
    let plan = Plan {
        evm_chain_id: parse("CHAIN_ID", value("CHAIN_ID"))?,
        axelar_chain_id: value("AXELAR_CHAIN_ID").into(),
        gateway_owner: parse("GATEWAY_OWNER", value("GATEWAY_OWNER"))?,
        operators_owner: parse("OPERATORS_OWNER", value("OPERATORS_OWNER"))?,
        gas_service_owner: parse("GAS_SERVICE_OWNER", value("GAS_SERVICE_OWNER"))?,
        its_owner: parse("ITS_OWNER", value("ITS_OWNER"))?,
        factory_owner: parse("FACTORY_OWNER", value("FACTORY_OWNER"))?,
        gateway_operator: parse("GATEWAY_OPERATOR", value("GATEWAY_OPERATOR"))?,
        prover_admin: derive_prover_admin(&lookup)?,
        approved_verifiers: saved
            .map(|plan| plan.approved_verifiers.clone())
            .unwrap_or_default(),
        evm_gas_budget: value("EVM_GAS_BUDGET").into(),
        cosmos_fee_budget: value("COSMOS_FEE_BUDGET").into(),
        reward_amount: value("REWARD_AMOUNT").into(),
        voting_threshold: saved.map_or(network.verifier_threshold(), |plan| plan.voting_threshold),
        signing_threshold: saved
            .map_or(network.verifier_threshold(), |plan| plan.signing_threshold),
        block_expiry: parse("BLOCK_EXPIRY", value("BLOCK_EXPIRY"))?,
        confirmation_height: parse("CONFIRMATION_HEIGHT", value("CONFIRMATION_HEIGHT"))?,
    };
    plan::validate(&plan)?;
    if let Some(saved) = saved {
        eyre::ensure!(
            &plan == saved,
            "deployment settings in .env differ from the saved plan; restore the original settings to resume"
        );
    }
    Ok(plan)
}

fn derive_prover_admin(lookup: &impl Fn(&str) -> Option<String>) -> Result<String> {
    let (name, mnemonic) =
        match lookup("MULTISIG_PROVER_MNEMONIC").filter(|mnemonic| !mnemonic.trim().is_empty()) {
            Some(mnemonic) => ("MULTISIG_PROVER_MNEMONIC", mnemonic),
            None => (
                "MNEMONIC",
                lookup("MNEMONIC")
                    .filter(|mnemonic| !mnemonic.trim().is_empty())
                    .ok_or_else(|| {
                        eyre::eyre!("missing MNEMONIC to derive the prover admin address")
                    })?,
            ),
        };
    derive_axelar_wallet(&mnemonic)
        .map(|(_, address)| address)
        .map_err(|_| eyre::eyre!("invalid {name}: cannot derive the prover admin address"))
}

fn parse<T: FromStr>(name: &str, value: &str) -> Result<T> {
    value.parse().map_err(|_| eyre::eyre!("invalid {name}"))
}
