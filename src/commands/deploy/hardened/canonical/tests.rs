use std::path::Path;

use alloy::sol_types::SolValue;
use alloy::{providers::ProviderBuilder, rpc::client::RpcClient, transports::mock::Asserter};

use super::*;
use crate::commands::deploy::{hardened::tests::initial_state, resolve_evm_key};

#[test]
fn mainnet_gateway_requires_exact_nonce_and_no_pending_transaction() {
    assert_eq!(GATEWAY_DEPLOYER.create(3), GATEWAY);
    assert!(validate_gateway_nonce(2, 2).is_ok());
    for (latest, pending) in [(0, 0), (1, 1), (2, 3), (3, 3), (4, 4)] {
        assert!(validate_gateway_nonce(latest, pending).is_err());
    }
}

#[test]
fn canonical_policy_rejects_different_addresses_without_restricting_testnet() {
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
        assert!(check_address(Network::Mainnet, name, expected(name).unwrap()).is_ok());
        assert!(check_address(Network::Mainnet, name, Address::ZERO).is_err());
        assert!(check_address(Network::Testnet, name, Address::repeat_byte(1)).is_ok());
    }
    assert_eq!(salt(Network::Mainnet, "Operators", "custom"), "Operators");
    assert_eq!(salt(Network::Testnet, "Operators", "custom"), "custom");
}

#[test]
fn mainnet_operators_use_the_canonical_deployment_account_not_gateway_account() {
    let mut state = initial_state();
    state.deployer_private_key = Some("deployer-key".into());
    state.gateway_deployer_private_key = Some("gateway-key".into());
    for step in [
        "Operators",
        "RegisterOperators",
        "TransferOperatorsOwnership",
    ] {
        state.env = Network::Testnet;
        assert_eq!(resolve_evm_key(&state, step).unwrap(), "gateway-key");
        state.env = Network::Mainnet;
        assert_eq!(resolve_evm_key(&state, step).unwrap(), "deployer-key");
    }
    assert_eq!(
        resolve_evm_key(&state, "AxelarGateway").unwrap(),
        "gateway-key"
    );
}

#[test]
fn mainnet_rejects_custom_its_salts_and_noncanonical_signer() {
    let mut state = initial_state();
    state.env = Network::Mainnet;
    state.its_salt = Some("v2.2.0".into());
    state.its_proxy_salt = Some("v1.0.0".into());
    state.predicted_gateway_address = None;
    assert!(validate_salts(&state).is_ok());
    state.its_proxy_salt = Some("custom".into());
    assert!(validate_salts(&state).is_err());
    state.deployer_private_key = Some(format!("{:064x}", 1));
    let error = validate_signers(&state).unwrap_err().to_string();
    assert!(error.contains("DEPLOYER_PRIVATE_KEY"));
    assert!(error.contains(&DEPLOYER.to_string()));
    assert!(!error.contains(state.deployer_private_key.as_ref().unwrap()));
}

#[tokio::test]
#[ignore = "requires the reference axelar-contract-deployments checkout and npm artifacts"]
async fn reference_artifacts_predict_the_existing_mainnet_addresses() {
    predictions::validate(Path::new("../axelar-contract-deployments"))
        .await
        .unwrap();
}

#[tokio::test]
async fn canonical_factory_rejects_missing_or_unexpected_code() {
    for code in ["0x", "0x60006000"] {
        let mock = Asserter::new();
        mock.push_success(&code);
        let provider = ProviderBuilder::new().connect_client(RpcClient::mocked(mock));
        let error = factory(&provider, "ConstAddressDeployer")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(if code == "0x" {
            "Bootstrap"
        } else {
            "unexpected runtime code"
        }));
    }
}

#[test]
fn canonical_create3_hash_reproduces_live_its_addresses() {
    assert_eq!(predictions::create3(DEPLOYER, "ITS v1.0.0"), ITS);
    assert_eq!(
        predictions::create3(DEPLOYER, "ITS Factory v1.0.0"),
        ITS_FACTORY
    );
    assert_ne!(predictions::create3(DEPLOYER, "ITS v2.2.0"), ITS);
    assert_ne!(predictions::create3(GATEWAY_DEPLOYER, "ITS v1.0.0"), ITS);
    assert_eq!(
        predictions::create3(DEPLOYER, "ITS v2.2.0 Implementation"),
        ITS_IMPLEMENTATION
    );
    assert_ne!(
        predictions::create3(DEPLOYER, "ITS v2.2.0"),
        ITS_IMPLEMENTATION
    );
}

#[test]
fn factory_implementation_reference_vector_requires_the_release_salt() {
    // ITS v2.2.0 factory creation bytecode plus the canonical ITS proxy constructor argument.
    let init_hash = b256!("33c4c45b5ea335ef79cdc91e0a5078b598e17897263936cdcbd027ce465ee8fd");
    let predict = |salt: &str| {
        let salt = keccak256((DEPLOYER, crate::evm::get_salt_from_key(salt)).abi_encode_params());
        CONST_DEPLOYER.create2(salt, init_hash)
    };
    assert_eq!(predict("ITS v2.2.0"), ITS_FACTORY_IMPLEMENTATION);
    assert_ne!(
        predict("ITS v2.2.0 Implementation"),
        ITS_FACTORY_IMPLEMENTATION
    );
}
