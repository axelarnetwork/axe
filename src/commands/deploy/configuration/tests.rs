use alloy::signers::local::PrivateKeySigner;
use bip32::{Language, Mnemonic};
use serde_json::json;

use super::{load_missing_environment, validate_init_environment, validate_state};
use crate::state::{State, StepStatus, default_steps};

fn complete_state() -> State {
    let key = PrivateKeySigner::random().to_bytes().to_string();
    serde_json::from_value(json!({
        "axelarId": "test-chain",
        "rpcUrl": "http://localhost:8545",
        "targetJson": "testnet.json",
        "mnemonic": Mnemonic::from_entropy([42; 32], Language::English).phrase(),
        "env": "testnet",
        "cosmSalt": "test",
        "deployerPrivateKey": key,
        "gatewayDeployerPrivateKey": key,
        "gasServiceDeployerPrivateKey": key,
        "itsDeployerPrivateKey": key,
        "itsSalt": "test",
        "itsProxySalt": "test",
        "proposals": {},
        "steps": default_steps()
    }))
    .unwrap()
}

#[test]
fn init_reports_all_missing_and_blank_variables() {
    let error = validate_init_environment(|name| match name {
        "GATEWAY_DEPLOYER_PRIVATE_KEY" | "ITS_SALT" => None,
        "ITS_PROXY_SALT" => Some("  ".into()),
        _ => Some("configured".into()),
    })
    .unwrap_err()
    .to_string();
    for name in ["GATEWAY_DEPLOYER_PRIVATE_KEY", "ITS_SALT", "ITS_PROXY_SALT"] {
        assert!(error.contains(name), "{error}");
    }
}

#[test]
fn run_reports_all_missing_credentials_and_salts() {
    let mut state = complete_state();
    state.deployer_private_key = None;
    state.gateway_deployer_private_key = None;
    state.gas_service_deployer_private_key = None;
    state.its_deployer_private_key = None;
    state.its_salt = Some(" ".into());
    state.its_proxy_salt = None;

    let error = validate_state(&mut state, None).unwrap_err().to_string();
    for name in [
        "DEPLOYER_PRIVATE_KEY",
        "GATEWAY_DEPLOYER_PRIVATE_KEY",
        "GAS_SERVICE_DEPLOYER_PRIVATE_KEY",
        "ITS_DEPLOYER_PRIVATE_KEY",
        "ITS_SALT",
        "ITS_PROXY_SALT",
    ] {
        assert!(error.contains(&format!("missing {name}")), "{error}");
    }
    assert!(state.gateway_deployer.is_none());
}

#[test]
fn invalid_credentials_are_reported_without_exposing_their_values() {
    let mut state = complete_state();
    state.mnemonic = "invalid mnemonic must not be printed".into();
    state.admin_mnemonic = Some("invalid admin mnemonic must not be printed".into());
    state.gateway_deployer_private_key = Some("invalid key must not be printed".into());
    state.rpc_url = "file:///tmp/rpc".into();

    let error = validate_state(&mut state, None).unwrap_err().to_string();
    for name in [
        "MNEMONIC",
        "MULTISIG_PROVER_MNEMONIC",
        "GATEWAY_DEPLOYER_PRIVATE_KEY",
        "RPC_URL",
    ] {
        assert!(error.contains(name), "{error}");
    }
    assert!(!error.contains("must not be printed"));
}

#[test]
fn repair_preserves_progress_and_existing_keys() {
    let mut state = complete_state();
    let key = state.deployer_private_key.clone().unwrap();
    let signer: PrivateKeySigner = key.parse().unwrap();
    state.steps[0].status = StepStatus::Completed;
    state.proposals.insert("instantiate".into(), 42);
    state.gateway_deployer_private_key = None;
    state.its_salt = None;
    state.its_proxy_salt = Some("  ".into());
    let steps = serde_json::to_value(&state.steps).unwrap();

    load_missing_environment(&mut state, |name| match name {
        "GATEWAY_DEPLOYER_PRIVATE_KEY" => Some(key.clone()),
        "ITS_SALT" | "ITS_PROXY_SALT" => Some("restored".into()),
        "DEPLOYER_PRIVATE_KEY" => Some("must not overwrite saved key".into()),
        _ => None,
    });
    validate_state(&mut state, None).unwrap();

    assert_eq!(state.gateway_deployer, Some(signer.address()));
    assert_eq!(state.deployer_private_key.as_deref(), Some(key.as_str()));
    assert_eq!(state.its_salt.as_deref(), Some("restored"));
    assert_eq!(serde_json::to_value(&state.steps).unwrap(), steps);
    assert_eq!(state.proposals.get("instantiate"), Some(&42));
}

#[test]
fn mismatched_gateway_key_cannot_change_the_saved_deployer() {
    let mut state = complete_state();
    let address = PrivateKeySigner::random().address();
    state.gateway_deployer = Some(address);

    let error = validate_state(&mut state, None).unwrap_err().to_string();

    assert!(error.contains("does not match"));
    assert_eq!(state.gateway_deployer, Some(address));
}

#[test]
fn private_key_override_can_supply_missing_role_keys() {
    let mut state = complete_state();
    let key = state.deployer_private_key.take().unwrap();
    state.gateway_deployer_private_key = None;
    state.gas_service_deployer_private_key = None;
    state.its_deployer_private_key = None;

    validate_state(&mut state, Some(&key)).unwrap();

    assert!(state.gateway_deployer.is_some());
    assert!(state.admin_mnemonic.is_none());
}
