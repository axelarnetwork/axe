use alloy::{primitives::Address, signers::local::PrivateKeySigner};

use super::*;
use crate::commands::deploy::{hardened::tests::initial_state, resolve_evm_key};

#[test]
fn mainnet_accepts_operator_selected_signers_and_gateway_prediction() {
    let mut state = initial_state();
    state.env = Network::Mainnet;
    state.its_salt = Some("v2.2.0".into());
    state.its_proxy_salt = Some("v1.0.0".into());
    // Public test keys: deliberately different roles and no historical Interop keys.
    state.deployer_private_key = Some(format!("{:064x}", 1));
    state.gateway_deployer_private_key = Some(format!("{:064x}", 2));
    state.gas_service_deployer_private_key = Some(format!("{:064x}", 3));
    state.its_deployer_private_key = Some(format!("{:064x}", 4));
    let gateway: PrivateKeySigner = state
        .gateway_deployer_private_key
        .as_ref()
        .unwrap()
        .parse()
        .unwrap();
    state.predicted_gateway_address = Some(gateway.address().create(43));
    assert!(validate(&state).is_ok());
    let predicted = state.predicted_gateway_address.unwrap();
    crate::commands::deploy::hardened::verification::validate_gateway_prediction(
        gateway.address(),
        42,
        predicted,
    )
    .unwrap();
    assert!(
        crate::commands::deploy::hardened::verification::validate_gateway_prediction(
            gateway.address(),
            43,
            predicted,
        )
        .is_err()
    );
    for (step, key) in [
        ("ConstAddressDeployer", 1),
        ("Create3Deployer", 1),
        ("Operators", 1),
        ("RegisterOperators", 1),
        ("TransferOperatorsOwnership", 1),
        ("AxelarGateway", 2),
        ("TransferGatewayOwnership", 2),
        ("AxelarGasService", 3),
        ("TransferGasServiceOwnership", 3),
        ("DeployInterchainTokenService", 4),
    ] {
        assert_eq!(
            resolve_evm_key(&state, step).unwrap(),
            format!("{key:064x}")
        );
    }
    assert_ne!(state.predicted_gateway_address.unwrap(), Address::ZERO);
    state.its_proxy_salt = Some("wrong-release".into());
    assert!(validate(&state).is_err());
}

#[test]
fn release_salts_are_independent_of_the_signer() {
    assert_eq!(salt(Network::Mainnet, "Operators", "custom"), "Operators");
    assert_eq!(salt(Network::Testnet, "Operators", "custom"), "custom");
}
