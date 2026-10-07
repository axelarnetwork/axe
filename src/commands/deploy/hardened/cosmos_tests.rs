use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use prost::Message;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::resume;
use crate::commands::deploy::hardened::types::Transaction;
use alloy::primitives::B256;

fn saved(raw: &[u8], hash: &str) -> Transaction {
    Transaction::Cosmos {
        intent: B256::ZERO,
        raw: raw.to_vec(),
        hash: hash.into(),
        sender: "test".into(),
        fee: "1".into(),
        sequence: 0,
    }
}

fn serve(status: &str, body: &str) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 4096];
        let count = stream.read(&mut request).unwrap();
        assert!(
            String::from_utf8_lossy(&request[..count]).starts_with("GET /cosmos/tx/v1beta1/txs/")
        );
        stream.write_all(response.as_bytes()).unwrap();
    });
    (url, server)
}

#[tokio::test]
async fn failed_lookup_is_never_interpreted_as_permission_to_broadcast() {
    let raw = b"recorded transaction";
    let hash = hex::encode_upper(Sha256::digest(raw));
    let (url, server) = serve("503 Service Unavailable", "{}");
    let error = resume(&url, &saved(raw, &hash), "test/cosmos")
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("cannot establish Cosmos transaction status")
    );
    server.join().unwrap();
}

#[tokio::test]
async fn included_transaction_is_reused_and_failure_is_not_retried() {
    let raw = b"recorded transaction";
    let hash = hex::encode_upper(Sha256::digest(raw));
    for code in [0, 7] {
        let body =
            json!({"tx_response":{"code":code,"height":"12","txhash":hash,"raw_log":"result"}})
                .to_string();
        let (url, server) = serve("200 OK", &body);
        let result = resume(&url, &saved(raw, &hash), "test/cosmos").await;
        assert_eq!(result.is_ok(), code == 0);
        server.join().unwrap();
    }
}

#[tokio::test]
async fn corrupted_signed_bytes_fail_before_any_network_request() {
    assert!(
        resume(
            "http://127.0.0.1:1",
            &saved(b"changed", "wrong-hash"),
            "test/cosmos"
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("signed bytes")
    );
}

#[test]
fn simulation_never_discloses_a_broadcastable_signature() {
    let tx = cosmos_sdk_proto::cosmos::tx::v1beta1::TxRaw {
        body_bytes: vec![1, 2],
        auth_info_bytes: vec![3, 4],
        signatures: vec![vec![42; 64]],
    };
    let bytes = super::unsigned_simulation(&tx.encode_to_vec()).unwrap();
    let simulation =
        cosmos_sdk_proto::cosmos::tx::v1beta1::TxRaw::decode(bytes.as_slice()).unwrap();
    assert_eq!(simulation.body_bytes, tx.body_bytes);
    assert_eq!(simulation.auth_info_bytes, tx.auth_info_bytes);
    assert_eq!(simulation.signatures, vec![vec![0; 64]]);
}

#[tokio::test]
async fn missing_receipt_with_consumed_sequence_never_broadcasts() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        for (path, status, body) in [
            ("/cosmos/tx/v1beta1/txs/", "404 Not Found", "{}"),
            (
                "/cosmos/auth/v1beta1/accounts/test",
                "200 OK",
                r#"{"account":{"account_number":"1","sequence":"1"}}"#,
            ),
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = [0; 4096];
            let size = stream.read(&mut bytes).unwrap();
            assert!(String::from_utf8_lossy(&bytes[..size]).starts_with(&format!("GET {path}")));
            write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
        // No listener remains for a broadcast. A resend would fail with a different error.
    });
    let auth = cosmos_sdk_proto::cosmos::tx::v1beta1::AuthInfo {
        signer_infos: vec![cosmos_sdk_proto::cosmos::tx::v1beta1::SignerInfo::default()],
        ..Default::default()
    };
    let raw = cosmos_sdk_proto::cosmos::tx::v1beta1::TxRaw {
        auth_info_bytes: auth.encode_to_vec(),
        ..Default::default()
    }
    .encode_to_vec();
    let hash = hex::encode_upper(Sha256::digest(&raw));
    let error = resume(&url, &saved(&raw, &hash), "test/cosmos")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no transaction was resent"));
    server.join().unwrap();
}
