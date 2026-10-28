use std::collections::BTreeMap;

use eyre::Result;

use crate::commands::deploy::{
    cosmos_signer,
    hardened::{governance, storage, types::Journal},
};
use crate::cosmos::{check_axelar_balance, derive_axelar_wallet};
use crate::state::{State, StepStatus};
use crate::ui;

#[cfg(test)]
mod tests;

pub(super) async fn check(
    state: &State,
    lcd: &str,
    chain: &str,
    denom: &str,
) -> Result<BTreeMap<&'static str, String>> {
    let plan = state
        .hardened_plan
        .as_ref()
        .ok_or_else(|| eyre::eyre!("missing plan"))?;
    let (_, proposer) =
        derive_axelar_wallet(cosmos_signer::mnemonic(state, "InstantiateChainContracts")?)?;
    let (_, admin) = derive_axelar_wallet(cosmos_signer::mnemonic(state, "WaitForVerifierSet")?)?;
    eyre::ensure!(
        admin == plan.prover_admin,
        "prover admin credential mismatch"
    );
    let funder = cosmos_signer::rewards_funder_address(state)?;
    ui::section("Axelar funding accounts");
    ui::address("Proposer (MNEMONIC)", &proposer);
    ui::address("Prover admin (MULTISIG_PROVER_MNEMONIC)", &admin);
    ui::address("Reward-pool funder (REWARDS_FUNDER_MNEMONIC)", &funder);
    ui::info(
        "The proposer pays proposal deposits and its fees. The reward-pool funder pays both pool deposits and its fees. Each distinct account needs its remaining COSMOS_FEE_BUDGET.",
    );
    let deposit = proposal_deposit(state, lcd, denom).await?;
    let path = storage::directory(state)?.join("journal.json");
    let journal: Option<Journal> = if path.exists() {
        Some(serde_json::from_slice(&tokio::fs::read(path).await?)?)
    } else {
        None
    };
    let recorded = |step: &str| {
        journal
            .as_ref()
            .is_some_and(|j| j.actions.contains_key(&format!("{step}/cosmos")))
    };
    let (remaining, escrowed) = super::proposal_deposit_status(state, &recorded);
    let rewards = pending_rewards(state, recorded("AddRewards"))?;
    let payments = payments_by_account(
        &proposer,
        &admin,
        &funder,
        super::sequential_deposit(remaining, escrowed, deposit),
        rewards,
    )?;
    for (address, payments) in payments {
        let fees = super::remaining_budget(state, &address, true).await?;
        let required = payments
            .checked_add(fees)
            .ok_or_else(|| eyre::eyre!("funding requirement overflow"))?;
        ui::address("Checking Axelar funding", &address);
        check_axelar_balance(
            lcd,
            chain,
            &address.parse::<cosmrs::AccountId>()?,
            &denom.parse::<cosmrs::Denom>()?,
            required,
        )
        .await?;
    }
    Ok(BTreeMap::from([
        ("proposer", proposer),
        ("MULTISIG_PROVER_MNEMONIC", admin),
        ("REWARDS_FUNDER_MNEMONIC", funder),
    ]))
}

async fn proposal_deposit(state: &State, lcd: &str, denom: &str) -> Result<u128> {
    let deposit = if state.env.deployment_uses_governance() {
        governance::deposit(lcd, denom).await?.parse()?
    } else {
        0
    };
    ui::kv(
        "proposal deposit (zero for direct execution)",
        &format!("{deposit} {denom} (base units)"),
    );
    if state.env.deployment_uses_governance() {
        ui::info(
            "This deposit is separate from transaction fees. Funding reserves one deposit for the sequential batches; reuse requires the preceding proposal to pass and its refund to arrive.",
        );
    } else {
        ui::info(
            "Devnet executes directly with no proposal deposit. The reward-pool funder still pays the pool deposits.",
        );
    }
    Ok(deposit)
}

fn pending_rewards(state: &State, recorded: bool) -> Result<u128> {
    if recorded
        || !state
            .steps
            .iter()
            .any(|s| s.name == "AddRewards" && s.status == StepStatus::Pending)
    {
        return Ok(0);
    }
    state
        .hardened_plan
        .as_ref()
        .ok_or_else(|| eyre::eyre!("missing plan"))?
        .reward_amount
        .parse::<u128>()?
        .checked_mul(2)
        .ok_or_else(|| eyre::eyre!("reward amount overflow"))
}

fn payments_by_account(
    proposer: &str,
    admin: &str,
    funder: &str,
    deposit: u128,
    rewards: u128,
) -> Result<BTreeMap<String, u128>> {
    let mut payments = BTreeMap::new();
    for (address, amount) in [(proposer, deposit), (admin, 0), (funder, rewards)] {
        let total = payments.entry(address.to_owned()).or_insert(0u128);
        *total = total
            .checked_add(amount)
            .ok_or_else(|| eyre::eyre!("funding requirement overflow"))?;
    }
    Ok(payments)
}
