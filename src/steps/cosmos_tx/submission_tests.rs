use super::{StepTxContext, messages};
use crate::{cosmos::build_execute_msg_any, types::Network};
use alloy::signers::local::PrivateKeySigner;
use cosmrs::crypto::secp256k1::SigningKey;
use serde_json::json;

#[tokio::test]
async fn devnet_submits_the_original_messages_without_querying_governance() {
    let signer = SigningKey::from_slice(PrivateKeySigner::random().to_bytes().as_slice()).unwrap();
    let address = signer
        .public_key()
        .account_id("axelar")
        .unwrap()
        .to_string();
    let tx = StepTxContext {
        signing_key: &signer,
        axelar_address: &address,
        lcd: "http://127.0.0.1:1",
        chain_id: "devnet-amplifier",
        fee_denom: "uaxl",
        gas_price: 0.007,
        chain_axelar_id: "test",
        env: "devnet-amplifier",
        proposal_key: "register",
    };
    let inner = vec![
        build_execute_msg_any(&address, "coordinator", &json!({"register_deployment":{}})).unwrap(),
        build_execute_msg_any(&address, "rewards", &json!({"create_pool":{}})).unwrap(),
    ];
    assert_eq!(
        messages(Network::DevnetAmplifier, tx, inner.clone(), "Register")
            .await
            .unwrap(),
        inner
    );
    // Governance networks must obtain a real deposit, and fail against this unreachable LCD.
    assert!(
        messages(Network::Testnet, tx, inner, "Register")
            .await
            .is_err()
    );
}
