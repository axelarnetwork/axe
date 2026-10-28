use serde_json::json;

use super::*;

#[test]
fn separate_reward_funder_removes_rewards_from_proposer_requirement() {
    let payments = payments_by_account("proposer", "admin", "funder", 400_000, 11_107).unwrap();
    assert_eq!(payments["proposer"], 400_000);
    assert_eq!(payments["funder"], 11_107);
    assert_eq!(payments["admin"], 0);
}

#[test]
fn shared_accounts_combine_payments_and_have_one_fee_budget() {
    let payments = payments_by_account("shared", "shared", "shared", 400_000, 11_107).unwrap();
    assert_eq!(payments.len(), 1);
    assert_eq!(payments["shared"], 411_107);
    assert!(payments_by_account("shared", "shared", "shared", u128::MAX, 1).is_err());
}

#[test]
fn resume_does_not_reserve_completed_or_journaled_pool_deposits_twice() {
    let mut state: State = serde_json::from_value(json!({
        "axelarId":"test-chain", "rpcUrl":"http://localhost", "targetJson":"testnet.json",
        "env":"testnet", "cosmSalt":"test", "steps":crate::state::default_steps(),
        "hardenedPlan":crate::commands::deploy::hardened::tests::example_plan()
    }))
    .unwrap();
    assert_eq!(pending_rewards(&state, false).unwrap(), 2_000_000);
    assert_eq!(pending_rewards(&state, true).unwrap(), 0);
    state
        .steps
        .iter_mut()
        .find(|step| step.name == "AddRewards")
        .unwrap()
        .status = StepStatus::Completed;
    assert_eq!(pending_rewards(&state, false).unwrap(), 0);
}
