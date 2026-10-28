use alloy::primitives::Address;
use bip32::{Language, Mnemonic};
use serde_json::json;

use super::*;
use crate::{
    commands::deploy::hardened::tests::initial_state, cosmos::derive_axelar_wallet, types::Network,
};

#[tokio::test]
async fn mainnet_proposal_uses_governance_upgrade_admin_and_codec_without_legacy_format() {
    let mut state = initial_state();
    state.env = Network::Mainnet;
    state.predicted_gateway_address = Some(Address::repeat_byte(7));
    let governor = "axelar10d07y265gmmuvt4z0w9aw880jnsr700j7v9daj";
    let plan = state.hardened_plan.as_ref().unwrap();
    let admin = plan.prover_admin.clone();
    let path = std::env::temp_dir().join(format!(
        "axe-instantiate-settings-{}.json",
        rand::random::<u64>()
    ));
    let config = json!({
        "chains":{"example":{"axelarId":"example"}},
        "axelar":{"chainId":"axelar-dojo-1","contracts":{
            "Router":{"address":"router"},
            "VotingVerifier":{"example":{
                "governanceAddress":governor,"serviceName":"amplifier",
                "sourceGatewayAddress":state.predicted_gateway_address.unwrap().to_string(),
                "votingThreshold":["2","3"],"blockExpiry":50,"confirmationHeight":1,
                "msgIdFormat":"hex_tx_hash_and_event_index","addressFormat":"eip55"
            }},
            "MultisigProver":{"example":{
                "governanceAddress":governor,"adminAddress":admin,
                "signingThreshold":["2","3"],"serviceName":"amplifier",
                "verifierSetDiffThreshold":0,"keyType":"ecdsa"
            }}
        }}
    });
    tokio::fs::write(&path, serde_json::to_vec(&config).unwrap())
        .await
        .unwrap();
    state.target_json = path.clone();
    let ctx = DeployContext {
        axelar_id: "example".into(),
        rpc_url: state.rpc_url.clone(),
        target_json: path.clone(),
        state,
    };
    let phrase = Mnemonic::from_entropy([42; 32], Language::English);
    let (key, signer) = derive_axelar_wallet(phrase.phrase()).unwrap();
    let tx = StepTxContext {
        signing_key: &key,
        axelar_address: &signer,
        lcd: "unused",
        chain_id: "axelar-dojo-1",
        fee_denom: "uaxl",
        gas_price: 0.01,
        chain_axelar_id: "example",
        env: "mainnet",
        proposal_key: "instantiate",
    };
    let addresses = ChainContractAddresses {
        coordinator: "coordinator".into(),
        rewards: "rewards".into(),
        multisig: "multisig".into(),
        codec: "evm-codec".into(),
        governance: governor.into(),
    };
    let payload = build_instantiate_plan(
        &ctx,
        &tx,
        &addresses,
        ChainCodeIds {
            gateway: 17,
            verifier: 64,
            prover: 55,
        },
    )
    .await
    .unwrap()
    .execute_msg;
    let manual = &payload["instantiate_chain_contracts"]["params"]["manual"];
    for role in ["gateway", "verifier", "prover"] {
        assert_eq!(manual[role]["contract_admin"], governor);
    }
    assert_eq!(manual["prover"]["msg"]["admin_address"], admin);
    assert_eq!(
        manual["verifier"]["msg"]["chain_codec_address"],
        "evm-codec"
    );
    assert!(manual["verifier"]["msg"].get("address_format").is_none());
    tokio::fs::remove_file(path).await.unwrap();
}
