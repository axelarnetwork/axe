use std::collections::BTreeMap;

use bip32::{Language, Mnemonic};

use super::load;
use crate::commands::deploy::hardened::{tests::example_plan as public_plan, types::Plan};
use crate::cosmos::derive_axelar_wallet;
use crate::types::Network;

fn mnemonic(seed: u8) -> String {
    Mnemonic::from_entropy([seed; 32], Language::English)
        .phrase()
        .to_string()
}

fn example_plan() -> Plan {
    let mut plan = public_plan();
    plan.voting_threshold = [51, 100];
    plan.signing_threshold = [51, 100];
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
        ("BLOCK_EXPIRY", "50".into()),
        ("CONFIRMATION_HEIGHT", "1".into()),
    ])
}

#[test]
fn environment_produces_the_same_plan_as_json_and_resumes_unchanged() {
    let values = environment();
    let first = load(Network::Testnet, None, |name| values.get(name).cloned()).unwrap();
    assert_eq!(first, example_plan());
    assert_eq!(
        load(Network::Testnet, Some(&first), |name| values
            .get(name)
            .cloned())
        .unwrap(),
        first
    );
}

#[test]
fn new_deployments_use_live_network_presets_without_reading_threshold_env() {
    let mut values = environment();
    values.insert("VOTING_THRESHOLD", "1/1".into());
    values.insert("SIGNING_THRESHOLD", "invalid obsolete value".into());
    for (network, expected) in [
        (Network::Mainnet, [2, 3]),
        (Network::Testnet, [51, 100]),
        (Network::Stagenet, [51, 100]),
        (Network::DevnetAmplifier, [6, 10]),
    ] {
        let plan = load(network, None, |name| {
            assert!(!matches!(name, "VOTING_THRESHOLD" | "SIGNING_THRESHOLD"));
            values.get(name).cloned()
        })
        .unwrap();
        assert_eq!(plan.voting_threshold, expected);
        assert_eq!(plan.signing_threshold, expected);
        assert_eq!(plan.reward_amount, values["REWARD_AMOUNT"]);
    }
}

#[test]
fn resume_preserves_saved_thresholds_instead_of_applying_new_network_defaults() {
    let values = environment();
    let mut saved = example_plan();
    saved.voting_threshold = [2, 3];
    saved.signing_threshold = [3, 4];
    let before = serde_json::to_vec(&saved).unwrap();
    for restored in [
        load(Network::Testnet, Some(&saved), |name| {
            values.get(name).cloned()
        })
        .unwrap(),
        load(Network::Testnet, Some(&saved), |_| None).unwrap(),
    ] {
        assert_eq!(serde_json::to_vec(&restored).unwrap(), before);
    }
}

#[test]
fn new_plan_cannot_override_network_thresholds() {
    let mut plan = example_plan();
    super::plan::validate_network_thresholds(&plan, Network::Testnet).unwrap();
    plan.signing_threshold = [2, 3];
    assert!(super::plan::validate_network_thresholds(&plan, Network::Testnet).is_err());
    plan.signing_threshold = [51, 100];
    plan.voting_threshold = [2, 3];
    assert!(super::plan::validate_network_thresholds(&plan, Network::Testnet).is_err());
}

#[test]
fn missing_fields_are_reported_together_including_explicit_empty_values() {
    let mut values = environment();
    values.remove("AXELAR_CHAIN_ID");
    values.insert("GATEWAY_OPERATOR", "  ".into());
    let error = load(Network::Testnet, None, |name| values.get(name).cloned())
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
        ("EVM_GAS_BUDGET", "0.05"),
        ("BLOCK_EXPIRY", "0"),
    ] {
        let mut values = environment();
        values.insert(name, value.into());
        assert!(
            load(Network::Testnet, None, |key| values.get(key).cloned()).is_err(),
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
        ("BLOCK_EXPIRY", "100"),
    ] {
        let mut values = environment();
        values.insert(name, value.into());
        let error = load(Network::Testnet, Some(&example_plan()), |key| {
            values.get(key).cloned()
        })
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
    let restored = load(Network::Testnet, Some(&saved), |name| {
        assert_ne!(name, "EXPECTED_INITIAL_VERIFIERS");
        values.get(name).cloned()
    })
    .unwrap();
    assert_eq!(serde_json::to_vec(&restored).unwrap(), before);
}

#[test]
fn saved_json_plan_can_resume_without_new_environment_but_partial_input_fails() {
    let saved = example_plan();
    assert_eq!(
        load(Network::Testnet, Some(&saved), |_| None).unwrap(),
        saved
    );
    assert!(load(Network::Testnet, None, |_| None).is_err());
    assert!(
        load(Network::Testnet, Some(&saved), |name| (name
            == "GATEWAY_OWNER")
            .then(String::new))
        .is_err()
    );
    assert!(
        load(Network::Testnet, Some(&saved), |name| (name == "CHAIN_ID")
            .then(|| "1301".into()))
        .is_err()
    );
}

#[test]
fn router_and_verifier_credentials_are_not_deployment_dependencies() {
    let values = environment();
    assert_eq!(
        load(Network::Testnet, None, |name| {
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
    let plan = load(Network::Testnet, None, |name| values.get(name).cloned()).unwrap();
    assert_eq!(plan.prover_admin, expected);
    assert_ne!(plan.prover_admin, example_plan().prover_admin);
    assert_eq!(
        load(Network::Testnet, Some(&plan), |name| values
            .get(name)
            .cloned())
        .unwrap(),
        plan
    );
}

#[test]
fn empty_admin_mnemonic_falls_back_to_proposer() {
    let mut values = environment();
    values.insert("MULTISIG_PROVER_MNEMONIC", "  ".into());
    assert_eq!(
        load(Network::Testnet, None, |name| values.get(name).cloned()).unwrap(),
        example_plan()
    );
}

#[test]
fn changed_admin_signer_cannot_replace_the_saved_identity() {
    for name in ["MNEMONIC", "MULTISIG_PROVER_MNEMONIC"] {
        let mut values = environment();
        let saved = load(Network::Testnet, None, |name| values.get(name).cloned()).unwrap();
        values.insert(name, mnemonic(43));
        let error = load(Network::Testnet, Some(&saved), |name| {
            values.get(name).cloned()
        })
        .unwrap_err();
        assert!(error.to_string().contains("differ"));
    }
}

#[test]
fn missing_or_invalid_admin_credentials_fail_without_exposing_them() {
    let mut values = environment();
    values.remove("MNEMONIC");
    let error = load(Network::Testnet, None, |name| values.get(name).cloned()).unwrap_err();
    assert!(error.to_string().contains("missing MNEMONIC"));
    let invalid = "sensitive invalid input";
    for name in ["MNEMONIC", "MULTISIG_PROVER_MNEMONIC"] {
        let mut values = environment();
        values.insert(name, invalid.into());
        let error = load(Network::Testnet, None, |name| values.get(name).cloned())
            .unwrap_err()
            .to_string();
        assert!(error.contains(name));
        assert!(!error.contains(invalid));
    }
}
