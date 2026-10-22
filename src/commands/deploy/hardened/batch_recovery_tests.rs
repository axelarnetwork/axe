use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;

use alloy::primitives::B256;
use cosmos_sdk_proto::cosmos::tx::v1beta1::{TxBody, TxRaw};
use prost::Message;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{
    governance, journal, plan, recovery_tests, session, storage,
    tests::initial_state,
    types::{Confirmation, Transaction},
};
use crate::commands::deploy::DeployContext;
use crate::cosmos::{build_execute_msg_any, build_submit_proposal_any};

fn combined_submission() -> (Vec<u8>, serde_json::Value) {
    let payloads = [
        json!({"instantiate_chain_contracts":{}}),
        json!({"register_deployment":{}}),
        json!({"create_pool":{"pool_id":{"contract":"verifier"}}}),
        json!({"create_pool":{"pool_id":{"contract":"multisig"}}}),
    ];
    let messages = payloads
        .iter()
        .map(|p| build_execute_msg_any("governance", "contract", p).unwrap())
        .collect();
    let proposal = build_submit_proposal_any(
        "proposer",
        messages,
        "Batch 1",
        "summary",
        "3000000000",
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
    let messages: Vec<_> = payloads.iter().map(|p|json!({"@type":"/cosmwasm.wasm.v1.MsgExecuteContract","sender":"governance","contract":"contract","funds":[],"msg":p})).collect();
    (
        raw,
        json!({"title":"Batch 1","status":"PROPOSAL_STATUS_PASSED","expedited":false,"messages":messages}),
    )
}

#[tokio::test]
async fn combined_proposal_recovers_after_submission_and_passage_without_rebroadcast() {
    let first = recovery_tests::session().await;
    let (raw, passed) = combined_submission();
    let hash = hex::encode_upper(Sha256::digest(&raw));
    let saved = Transaction::Cosmos {
        raw,
        hash: hash.clone(),
        intent: B256::ZERO,
        sender: "proposer".into(),
        fee: "1".into(),
        sequence: 7,
    };
    first
        .record("InstantiateChainContracts/cosmos".into(), saved.clone())
        .await
        .unwrap();
    let response = json!({"tx_response":{"code":0,"height":"123","txhash":hash,"events":[{"type":"submit_proposal","attributes":[{"key":"proposal_id","value":"42"}]}]}});
    session::scope(
        first.clone(),
        journal::confirm(
            hash,
            Confirmation::Cosmos {
                height: 123,
                code: 0,
                response,
            },
        ),
    )
    .await
    .unwrap();
    // Only the proposal GET is allowed. A transaction POST/rebroadcast fails the assertion.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let lcd = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = [0; 4096];
        let size = socket.read(&mut request).unwrap();
        assert!(
            String::from_utf8_lossy(&request[..size])
                .starts_with("GET /cosmos/gov/v1/proposals/42 ")
        );
        let body = json!({"proposal":passed}).to_string();
        write!(socket,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
    });
    let mut state = initial_state();
    state.steps = plan::steps(&first.plan).unwrap();
    state.target_json = first.path.parent().unwrap().join("config.json");
    storage::atomic_write(&state.target_json,&serde_json::to_vec(&json!({"chains":{},"axelar":{"lcd":lcd,"chainId":"axelar-testnet-lisbon-3","gasPrice":"0.007uaxl"}})).unwrap()).unwrap();
    let mut ctx = DeployContext {
        axelar_id: state.axelar_id.to_string(),
        rpc_url: state.rpc_url.clone(),
        target_json: state.target_json.clone(),
        state,
    };
    let restored = Arc::new(
        session::Session::load(
            first.path.clone(),
            B256::ZERO,
            first.plan.clone(),
            first.rpc.clone(),
        )
        .await
        .unwrap(),
    );
    session::scope(restored.clone(), async {
        assert!(
            governance::recover(&mut ctx, "InstantiateChainContracts", "instantiate")
                .await
                .unwrap()
        );
        assert_eq!(ctx.state.proposals.get("instantiate"), Some(&42));
        governance::check(&ctx, "instantiate").await.unwrap();
        assert!(
            governance::recover(&mut ctx, "InstantiateChainContracts", "instantiate")
                .await
                .unwrap()
        );
    })
    .await;
    assert_eq!(restored.journal.lock().await.actions.len(), 1);
    server.join().unwrap();
    std::fs::remove_dir_all(first.path.parent().unwrap()).unwrap();
}
