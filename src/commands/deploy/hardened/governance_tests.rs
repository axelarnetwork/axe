use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use base64::Engine;
use cosmos_sdk_proto::cosmos::tx::v1beta1::{TxBody, TxRaw};
use prost::Message;
use serde_json::{Value, json};

use super::{validate_payload, wait_until_voting_ends};
use crate::cosmos::{build_execute_msg_any, build_submit_proposal_any};

fn proposal() -> (Vec<u8>, Value) {
    let payload = json!({"register_deployment":{"deployment_name":"example-24-87-85"}});
    let execute = build_execute_msg_any("governance", "coordinator", &payload).unwrap();
    let proposal = build_submit_proposal_any(
        "proposer",
        vec![execute],
        "Register",
        "summary",
        "400",
        "uaxl",
        true,
    )
    .unwrap();
    let body = TxBody {
        messages: vec![proposal],
        ..Default::default()
    };
    let raw = TxRaw {
        body_bytes: body.encode_to_vec(),
        ..Default::default()
    }
    .encode_to_vec();
    (
        raw,
        json!({"title":"Register","expedited":true,"messages":[{
            "@type":"/cosmwasm.wasm.v1.MsgExecuteContract", "sender":"governance", "contract":"coordinator", "funds":[],
            "msg":base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&payload).unwrap())
        }]}),
    )
}

#[test]
fn proposal_must_match_approved_messages_and_expedited_mode() {
    let (raw, actual) = proposal();
    validate_payload(&raw, &actual).unwrap();
    for changed in [
        json!({"contract":"another-coordinator"}),
        json!({"sender":"another-authority"}),
        json!({"funds":[{"denom":"uaxl","amount":"1"}]}),
        json!({"msg":{"register_deployment":{"deployment_name":"another-deployment"}}}),
    ] {
        let mut invalid = actual.clone();
        for (key, value) in changed.as_object().unwrap() {
            invalid["messages"][0][key] = value.clone();
        }
        assert!(validate_payload(&raw, &invalid).is_err());
    }
    let mut invalid = actual.clone();
    invalid["expedited"] = json!(false);
    assert!(validate_payload(&raw, &invalid).is_err());
    invalid = actual;
    invalid["messages"].as_array_mut().unwrap().clear();
    assert!(validate_payload(&raw, &invalid).is_err());
}

#[test]
fn proposal_accepts_equivalent_decoded_json_payload() {
    let (raw, mut actual) = proposal();
    actual["messages"][0]["msg"] =
        json!({"register_deployment":{"deployment_name":"example-24-87-85"}});
    validate_payload(&raw, &actual).unwrap();
}

fn proposal_server(responses: Vec<(u16, Value)>) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let lcd = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        for (status, proposal) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 4096];
            let size = stream.read(&mut request).unwrap();
            assert!(
                String::from_utf8_lossy(&request[..size])
                    .starts_with("GET /cosmos/gov/v1/proposals/653 HTTP/1.1")
            );
            let body = json!({"proposal":proposal}).to_string();
            write!(stream, "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    (lcd, server)
}

#[tokio::test]
async fn governance_wait_polls_same_proposal_until_voting_ends() {
    let voting = json!({"status":"PROPOSAL_STATUS_VOTING_PERIOD"});
    for status in [
        "PROPOSAL_STATUS_PASSED",
        "PROPOSAL_STATUS_REJECTED",
        "PROPOSAL_STATUS_FAILED",
        "PROPOSAL_STATUS_DEPOSIT_PERIOD",
    ] {
        let outcome = json!({"status":status,"messages":[{"msg":"final contents"}]});
        let (lcd, server) = proposal_server(vec![(200, voting.clone()), (200, outcome.clone())]);
        let actual = wait_until_voting_ends(&lcd, 653, voting.clone(), Duration::from_millis(1))
            .await
            .unwrap();
        assert_eq!(actual, outcome);
        server.join().unwrap();
    }
}

#[tokio::test]
async fn governance_wait_propagates_lcd_errors_without_replacing_proposal() {
    let (lcd, server) = proposal_server(vec![(500, Value::Null)]);
    let result = wait_until_voting_ends(
        &lcd,
        653,
        json!({"status":"PROPOSAL_STATUS_VOTING_PERIOD"}),
        Duration::from_millis(1),
    )
    .await;
    assert!(result.is_err());
    server.join().unwrap();
}

#[tokio::test]
async fn already_resolved_proposal_needs_no_poll() {
    let passed = json!({"status":"PROPOSAL_STATUS_PASSED"});
    let actual = wait_until_voting_ends(
        "http://127.0.0.1:1",
        653,
        passed.clone(),
        Duration::from_secs(15),
    )
    .await
    .unwrap();
    assert_eq!(actual, passed);
}
