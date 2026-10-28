use std::sync::Arc;

use alloy::primitives::B256;
use cosmos_sdk_proto::{
    cosmos::tx::v1beta1::{TxBody, TxRaw},
    cosmwasm::wasm::v1::MsgExecuteContract,
};
use prost::Message;
use serde_json::{Value, json};

use super::*;
use crate::commands::deploy::hardened::{governance, tests::example_plan};
use crate::cosmos::build_submit_proposal_any;

fn prediction() -> CosmosDeployment {
    CosmosDeployment {
        deployment_name: "example-24-87-85".into(),
        coordinator: "coordinator".into(),
        salt: "same-salt".into(),
        code_ids: [24, 87, 85],
        gateway: "gateway".into(),
        verifier: "predicted-verifier".into(),
        prover: "prover".into(),
    }
}

#[test]
fn combined_proposal_preserves_order_authority_and_predicted_pool_target() {
    let addresses = ChainContractAddresses {
        coordinator: "coordinator".into(),
        rewards: "rewards".into(),
        multisig: "multisig".into(),
        codec: "codec".into(),
        governance: "governance".into(),
    };
    let mut messages = vec![
        build_execute_msg_any(
            "governance",
            "coordinator",
            &json!({"instantiate_chain_contracts":{"salt":"same-salt"}}),
        )
        .unwrap(),
    ];
    messages.extend(
        registration_messages(
            "governance",
            "mainnet",
            "example",
            &addresses,
            &prediction(),
        )
        .unwrap(),
    );
    let decoded: Vec<_> = messages
        .iter()
        .map(|m| MsgExecuteContract::decode(m.value.as_slice()).unwrap())
        .collect();
    assert_eq!(decoded.len(), 4);
    assert!(
        decoded
            .iter()
            .all(|m| m.sender == "governance" && m.funds.is_empty())
    );
    assert_eq!(
        decoded
            .iter()
            .map(|m| m.contract.as_str())
            .collect::<Vec<_>>(),
        ["coordinator", "coordinator", "rewards", "rewards"]
    );
    let payloads: Vec<Value> = decoded
        .iter()
        .map(|m| serde_json::from_slice(&m.msg).unwrap())
        .collect();
    assert!(payloads[0].get("instantiate_chain_contracts").is_some());
    assert_eq!(
        payloads[1]["register_deployment"]["deployment_name"],
        prediction().deployment_name
    );
    assert_eq!(
        payloads[2]["create_pool"]["pool_id"]["contract"],
        "predicted-verifier"
    );
    assert_eq!(
        payloads[3]["create_pool"]["pool_id"]["contract"],
        "multisig"
    );
    assert_eq!(
        payloads[2]["create_pool"]["params"]["rewards_per_epoch"],
        "5553500000"
    );
    let proposal = build_submit_proposal_any(
        "proposer",
        messages,
        "Batch 1",
        "summary",
        "400000000000",
        "uaxl",
        true,
    )
    .unwrap();
    let raw = TxRaw {
        body_bytes: TxBody {
            messages: vec![proposal],
            ..Default::default()
        }
        .encode_to_vec(),
        ..Default::default()
    }
    .encode_to_vec();
    let actual: Vec<_> = decoded.iter().zip(payloads).map(|(m,p)| json!({"@type":"/cosmwasm.wasm.v1.MsgExecuteContract","sender":m.sender,"contract":m.contract,"funds":[],"msg":p})).collect();
    let mut response = json!({"title":"Batch 1","expedited":true,"messages":actual});
    governance::validate_payload(&raw, &response).unwrap();
    response["messages"].as_array_mut().unwrap().swap(0, 1);
    assert!(governance::validate_payload(&raw, &response).is_err());
}

#[test]
fn only_an_exact_pool_not_found_error_allows_creation() {
    assert!(pool_not_found(
        r#"{"code":2,"message":"rewards pool not found: query wasm contract failed"}"#
    ));
    for body in [
        "not found",
        r#"{"code":2,"message":"invalid chain: query wasm contract failed"}"#,
        r#"{"code":5,"message":"rewards pool not found: query wasm contract failed"}"#,
        r#"{"code":2,"message":"no such contract: query wasm contract failed"}"#,
    ] {
        assert!(!pool_not_found(body));
    }
}

#[tokio::test]
async fn predictions_survive_restart_and_cannot_change_after_preparation() {
    let path = std::env::temp_dir()
        .join(format!("axe-batch-{}", rand::random::<u64>()))
        .join("journal.json");
    let original = Arc::new(
        session::Session::load(path.clone(), B256::ZERO, example_plan(), String::new())
            .await
            .unwrap(),
    );
    session::scope(original, pin_predictions(&prediction()))
        .await
        .unwrap();
    let restored = Arc::new(
        session::Session::load(path.clone(), B256::ZERO, example_plan(), String::new())
            .await
            .unwrap(),
    );
    assert!(restored.journal.lock().await.actions.is_empty()); // preparation/declined approval cannot broadcast
    session::scope(restored, async {
        pin_predictions(&prediction()).await.unwrap();
        let mut changed = prediction();
        changed.verifier = "other".into();
        assert!(pin_predictions(&changed).await.is_err());
    })
    .await;
    let expected = prediction();
    let mut actual = json!({"chain_name":"example","gateway_address":"gateway","verifier_address":"predicted-verifier","prover_address":"prover"});
    crate::steps::cosmos_query::validate_prediction(
        &expected,
        &expected.deployment_name,
        "example",
        &actual,
    )
    .unwrap();
    for field in [
        "gateway_address",
        "verifier_address",
        "prover_address",
        "chain_name",
    ] {
        let before = actual[field].clone();
        actual[field] = json!("wrong");
        assert!(
            crate::steps::cosmos_query::validate_prediction(
                &expected,
                &expected.deployment_name,
                "example",
                &actual
            )
            .is_err()
        );
        actual[field] = before;
    }
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
