use std::collections::BTreeMap;

use bip32::{Language, Mnemonic};

use super::load;
use crate::commands::deploy::hardened::{tests::example_plan as public_plan, types::Plan};
use crate::cosmos::derive_axelar_wallet;

fn mnemonic(seed: u8) -> String {
    Mnemonic::from_entropy([seed; 32], Language::English)
        .phrase()
        .to_string()
}

fn example_plan() -> Plan {
    let mut plan = public_plan();
    plan.prover_admin = derive_axelar_wallet(&mnemonic(42)).unwrap().1;
    plan
}

fn environment() -> BTreeMap<&'static str, String> {
    let plan = example_plan();
    BTreeMap::from([
        ("CHAIN_ID", "31337".into()),
        ("AXELAR_CHAIN_ID", plan.axelar_chain_id),
        ("GATEWAY_OWNER", plan.gateway_owner.to_string()),
        ("OPERATORS_OWNER", plan.operators_owner.to_string()),
        ("GAS_SERVICE_OWNER", plan.gas_service_owner.to_string()),
        ("ITS_OWNER", plan.its_owner.to_string()),
        ("FACTORY_OWNER", plan.factory_owner.to_string()),
        ("GATEWAY_OPERATOR", plan.gateway_operator.to_string()),
        ("MNEMONIC", mnemonic(42)),
        ("EVM_GAS_BUDGET", plan.evm_gas_budget),
        ("COSMOS_FEE_BUDGET", plan.cosmos_fee_budget),
        ("REWARD_AMOUNT", plan.reward_amount),
        ("VOTING_THRESHOLD", "2/3".into()),
        ("SIGNING_THRESHOLD", "2/3".into()),
        ("BLOCK_EXPIRY", "50".into()),
        ("CONFIRMATION_HEIGHT", "1".into()),
    ])
}

#[test]
fn environment_produces_the_same_plan_as_json_and_resumes_unchanged() {
    let mut values = environment();
    values.insert("VOTING_THRESHOLD", " 2 / 3 ".into());
    let first = load(None, |name| values.get(name).cloned()).unwrap();
    assert_eq!(first, example_plan());
    assert_eq!(
        load(Some(&first), |name| values.get(name).cloned()).unwrap(),
        first
    );
}

#[test]
fn missing_fields_are_reported_together_including_explicit_empty_values() {
    let mut values = environment();
    values.remove("AXELAR_CHAIN_ID");
    values.insert("GATEWAY_OPERATOR", "  ".into());
    let error = load(None, |name| values.get(name).cloned())
        .unwrap_err()
        .to_string();
    for name in ["AXELAR_CHAIN_ID", "GATEWAY_OPERATOR"] {
        assert!(error.contains(name), "{error}");
    }
}

#[test]
fn invalid_values_are_rejected_before_deployment() {
    for (name, value) in [
        ("GATEWAY_OWNER", "not-an-address"),
        ("CHAIN_ID", "wrong-chain"),
        ("VOTING_THRESHOLD", "2:3"),
        ("VOTING_THRESHOLD", "1/2"),
        ("SIGNING_THRESHOLD", "2/3/4"),
        ("EVM_GAS_BUDGET", "0.05"),
        ("BLOCK_EXPIRY", "0"),
    ] {
        let mut values = environment();
        values.insert(name, value.into());
        assert!(
            load(None, |key| values.get(key).cloned()).is_err(),
            "{name}"
        );
    }
}

#[test]
fn changed_settings_cannot_replace_the_saved_plan() {
    for (name, value) in [
        ("CHAIN_ID", "1301"),
        (
            "GATEWAY_OWNER",
            "0x0909090909090909090909090909090909090909",
        ),
        ("AXELAR_CHAIN_ID", "another-network"),
        ("COSMOS_FEE_BUDGET", "2000000"),
        ("SIGNING_THRESHOLD", "3/4"),
        ("BLOCK_EXPIRY", "100"),
    ] {
        let mut values = environment();
        values.insert(name, value.into());
        let error = load(Some(&example_plan()), |key| values.get(key).cloned())
            .unwrap_err()
            .to_string();
        assert!(error.contains("differ"), "{name}: {error}");
    }
}

#[test]
fn obsolete_verifier_env_is_not_read_and_old_journal_plan_is_preserved() {
    let values = environment();
    let mut saved = example_plan();
    saved.approved_verifiers = vec!["axelar1vykg4kxuanj87nsx7qllxuqxt2gk3g0lfgs29h".into()];
    let before = serde_json::to_vec(&saved).unwrap();
    let restored = load(Some(&saved), |name| {
        assert_ne!(name, "EXPECTED_INITIAL_VERIFIERS");
        values.get(name).cloned()
    })
    .unwrap();
    assert_eq!(serde_json::to_vec(&restored).unwrap(), before);
}

#[test]
fn saved_json_plan_can_resume_without_new_environment_but_partial_input_fails() {
    let saved = example_plan();
    assert_eq!(load(Some(&saved), |_| None).unwrap(), saved);
    assert!(load(None, |_| None).is_err());
    assert!(
        load(Some(&saved), |name| (name == "GATEWAY_OWNER")
            .then(String::new))
        .is_err()
    );
    assert!(
        load(Some(&saved), |name| (name == "CHAIN_ID")
            .then(|| "1301".into()))
        .is_err()
    );
}

#[test]
fn router_and_verifier_credentials_are_not_deployment_dependencies() {
    let values = environment();
    assert_eq!(
        load(None, |name| {
            assert!(!matches!(
                name,
                "PROVER_ADMIN"
                    | "ROUTER_ADMIN"
                    | "ROUTER_ADMIN_MNEMONIC"
                    | "VERIFIER_ADDRESS"
                    | "VERIFIER_MNEMONIC"
            ));
            values.get(name).cloned()
        })
        .unwrap(),
        example_plan()
    );
}

#[test]
fn explicit_admin_mnemonic_selects_a_separate_account() {
    let mut values = environment();
    let admin = mnemonic(43);
    let expected = derive_axelar_wallet(&admin).unwrap().1;
    values.insert("MULTISIG_PROVER_MNEMONIC", admin);
    let plan = load(None, |name| values.get(name).cloned()).unwrap();
    assert_eq!(plan.prover_admin, expected);
    assert_ne!(plan.prover_admin, example_plan().prover_admin);
    assert_eq!(
        load(Some(&plan), |name| values.get(name).cloned()).unwrap(),
        plan
    );
}

#[test]
fn empty_admin_mnemonic_falls_back_to_proposer() {
    let mut values = environment();
    values.insert("MULTISIG_PROVER_MNEMONIC", "  ".into());
    assert_eq!(
        load(None, |name| values.get(name).cloned()).unwrap(),
        example_plan()
    );
}

#[test]
fn changed_admin_signer_cannot_replace_the_saved_identity() {
    for name in ["MNEMONIC", "MULTISIG_PROVER_MNEMONIC"] {
        let mut values = environment();
        let saved = load(None, |name| values.get(name).cloned()).unwrap();
        values.insert(name, mnemonic(43));
        let error = load(Some(&saved), |name| values.get(name).cloned()).unwrap_err();
        assert!(error.to_string().contains("differ"));
    }
}

#[test]
fn missing_or_invalid_admin_credentials_fail_without_exposing_them() {
    let mut values = environment();
    values.remove("MNEMONIC");
    let error = load(None, |name| values.get(name).cloned()).unwrap_err();
    assert!(error.to_string().contains("missing MNEMONIC"));
    let invalid = "sensitive invalid input";
    for name in ["MNEMONIC", "MULTISIG_PROVER_MNEMONIC"] {
        let mut values = environment();
        values.insert(name, invalid.into());
        let error = load(None, |name| values.get(name).cloned())
            .unwrap_err()
            .to_string();
        assert!(error.contains(name));
        assert!(!error.contains(invalid));
    }
}
