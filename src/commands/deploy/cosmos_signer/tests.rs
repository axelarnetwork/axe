use bip32::{Language, Mnemonic};
use serde_json::json;

use super::*;

fn state() -> State {
    let mut state: State = serde_json::from_value(json!({
        "axelarId":"test-chain", "rpcUrl":"http://localhost", "targetJson":"testnet.json",
        "env":"testnet", "cosmSalt":"test", "steps":[]
    }))
    .unwrap();
    state.mnemonic = Mnemonic::from_entropy([1; 32], Language::English)
        .phrase()
        .into();
    state.admin_mnemonic = Some(
        Mnemonic::from_entropy([2; 32], Language::English)
            .phrase()
            .into(),
    );
    state.rewards_funder_mnemonic = Some(
        Mnemonic::from_entropy([3; 32], Language::English)
            .phrase()
            .into(),
    );
    state
}

#[test]
fn each_step_and_fee_replacement_selects_its_own_cosmos_signer() {
    let state = state();
    for (action, expected) in [
        ("InstantiateChainContracts/cosmos", state.mnemonic.as_str()),
        ("RegisterItsOnHub/cosmos", state.mnemonic.as_str()),
        (
            "WaitForVerifierSet/cosmos",
            state.admin_mnemonic.as_deref().unwrap(),
        ),
        (
            "AddRewards/cosmos",
            state.rewards_funder_mnemonic.as_deref().unwrap(),
        ),
    ] {
        let step = action.strip_suffix("/cosmos").unwrap();
        assert_eq!(mnemonic(&state, step).unwrap(), expected);
    }
    assert!(mnemonic(&state, "unknown").is_err());
}

#[test]
fn missing_reward_funder_never_falls_back_to_proposer() {
    let mut state = state();
    for missing in [None, Some(" ".into())] {
        state.rewards_funder_mnemonic = missing;
        assert!(
            mnemonic(&state, "AddRewards")
                .unwrap_err()
                .to_string()
                .contains("REWARDS_FUNDER_MNEMONIC")
        );
    }
}

#[test]
fn mainnet_requires_the_three_selected_accounts() {
    for (name, expected) in [
        ("MNEMONIC", MAINNET_PROPOSER),
        ("MULTISIG_PROVER_MNEMONIC", MAINNET_PROVER_ADMIN),
        ("REWARDS_FUNDER_MNEMONIC", MAINNET_REWARDS_FUNDER),
    ] {
        expected.parse::<cosmrs::AccountId>().unwrap();
        validate_address(name, expected, expected).unwrap();
        let error = validate_address(name, "wrong-account", expected)
            .unwrap_err()
            .to_string();
        assert!(error.contains(name));
        assert!(error.contains(expected));
    }
    let mut state = state();
    state.env = Network::Mainnet;
    assert!(
        validate_mainnet(&state)
            .unwrap_err()
            .to_string()
            .contains(MAINNET_PROPOSER)
    );
    assert!(
        rewards_funder_address(&state)
            .unwrap_err()
            .to_string()
            .contains(MAINNET_REWARDS_FUNDER)
    );
}

#[test]
fn other_networks_allow_explicit_operator_selected_accounts() {
    let mut state = state();
    for network in [
        Network::Testnet,
        Network::Stagenet,
        Network::DevnetAmplifier,
    ] {
        state.env = network;
        validate_mainnet(&state).unwrap();
        assert_eq!(
            rewards_funder_address(&state).unwrap(),
            derive_axelar_wallet(state.rewards_funder_mnemonic.as_deref().unwrap())
                .unwrap()
                .1
        );
    }
}
