use base64::Engine;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

use alloy::primitives::B256;
use alloy::signers::local::PrivateKeySigner;
use cosmos_sdk_proto::cosmos::tx::v1beta1::{AuthInfo, TxRaw};
use cosmrs::crypto::secp256k1::SigningKey;
use prost::Message;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{
    cosmos_fees, cosmos_funding, cosmos_recovery, journal, session,
    tests::example_plan,
    types::{Confirmation, Transaction},
};
use crate::cosmos::tx::{CosmosTxSignInput, build_and_sign_cosmos_tx};

fn key() -> SigningKey {
    SigningKey::from_slice(PrivateKeySigner::random().to_bytes().as_slice()).unwrap()
}

fn original(signer: &SigningKey, sequence: u64) -> Transaction {
    let sender = signer
        .public_key()
        .account_id("axelar")
        .unwrap()
        .to_string();
    let message =
        crate::cosmos::build_execute_msg_any(&sender, "target", &json!({"add_rewards":{}}))
            .unwrap();
    let raw = build_and_sign_cosmos_tx(
        signer,
        &CosmosTxSignInput {
            chain_id: "test-chain",
            account_number: 9,
            sequence,
            gas_limit: 100_000,
            fee_amount: 100,
            fee_denom: "uaxl",
            messages: vec![message],
        },
    )
    .unwrap();
    Transaction::Cosmos {
        hash: hex::encode_upper(Sha256::digest(&raw)),
        raw,
        sender,
        sequence,
        fee: "100".into(),
        intent: B256::ZERO,
    }
}

#[test]
fn fee_replacement_preserves_body_sequence_and_all_other_auth_fields() {
    let signer = key();
    let old = original(&signer, 7);
    let new = cosmos_fees::resign(&old, &signer, "test-chain", 9, 300).unwrap();
    let Transaction::Cosmos {
        raw: a,
        hash: old_hash,
        sender,
        sequence,
        ..
    } = &old
    else {
        unreachable!()
    };
    let Transaction::Cosmos {
        raw: b,
        hash: new_hash,
        sender: new_sender,
        sequence: new_sequence,
        ..
    } = &new
    else {
        unreachable!()
    };
    let a = TxRaw::decode(a.as_slice()).unwrap();
    let b = TxRaw::decode(b.as_slice()).unwrap();
    assert_eq!(a.body_bytes, b.body_bytes);
    assert_eq!(sender, new_sender);
    assert_eq!(sequence, new_sequence);
    assert_ne!(old_hash, new_hash);
    assert_ne!(a.signatures, b.signatures);
    let mut auth = AuthInfo::decode(a.auth_info_bytes.as_slice()).unwrap();
    auth.fee.as_mut().unwrap().amount[0].amount = "300".into();
    assert_eq!(auth.encode_to_vec(), b.auth_info_bytes);
    // Verifying the replacement's signature permits another fee bump in the same domain.
    cosmos_fees::resign(&new, &signer, "test-chain", 9, 400).unwrap();
    for (chain, account, fee) in [
        ("wrong-chain", 9, 300),
        ("test-chain", 10, 300),
        ("test-chain", 9, 100),
    ] {
        assert!(cosmos_fees::resign(&old, &signer, chain, account, fee).is_err());
    }
    assert!(cosmos_fees::resign(&old, &key(), "test-chain", 9, 300).is_err());
}

#[tokio::test]
async fn original_cosmos_attempt_can_win_after_replacement_and_restart() {
    let signer = key();
    let old = original(&signer, 7);
    let replacement = cosmos_fees::resign(&old, &signer, "test-chain", 9, 300).unwrap();
    let directory = std::env::temp_dir().join(format!("axe-cosmos-fees-{}", rand::random::<u64>()));
    let session = Arc::new(
        session::Session::load(
            directory.join("journal.json"),
            B256::ZERO,
            example_plan(),
            "unused".into(),
        )
        .await
        .unwrap(),
    );
    session
        .record("AddRewards/cosmos".into(), old.clone())
        .await
        .unwrap();
    session::scope(
        session.clone(),
        journal::replace("AddRewards/cosmos", replacement.clone()),
    )
    .await
    .unwrap();
    let Transaction::Cosmos { hash, sender, .. } = old else {
        unreachable!()
    };
    let response = json!({"tx_response":{"code":0,"height":"12","txhash":hash}});
    session::scope(
        session.clone(),
        journal::confirm(
            hash,
            Confirmation::Cosmos {
                height: 12,
                code: 0,
                response: response.clone(),
            },
        ),
    )
    .await
    .unwrap();
    let restored = reopen(&session).await;
    let result = session::scope(
        restored.clone(),
        cosmos_recovery::resume("http://127.0.0.1:1", &replacement, "AddRewards/cosmos"),
    )
    .await
    .unwrap();
    assert_eq!(result, response);
    let mut journal = restored.journal.lock().await;
    assert_eq!(
        cosmos_funding::fee_liability(&journal, &sender, None).unwrap(),
        300
    );
    journal
        .attempts
        .get_mut("AddRewards/cosmos")
        .unwrap()
        .push(original(&signer, 6));
    assert_eq!(
        cosmos_funding::fee_liability(&journal, &sender, None).unwrap(),
        400
    );
    assert_eq!(
        cosmos_funding::fee_liability(&journal, &sender, Some((7, 500))).unwrap(),
        600
    );
    std::fs::remove_dir_all(directory).unwrap();
}

fn serve(
    responses: Vec<(String, Option<serde_json::Value>)>,
    expected_raw: &[u8],
) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let expected = base64::engine::general_purpose::STANDARD.encode(expected_raw);
    let server = std::thread::spawn(move || {
        for (request_line, response) in responses {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line.trim(), format!("{request_line} HTTP/1.1"));
            let mut length = 0;
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            if request_line.starts_with("POST ") {
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(request["tx_bytes"], expected);
            }
            let status = if response.is_some() {
                "200 OK"
            } else {
                "404 Not Found"
            };
            let body = response.unwrap_or(json!({})).to_string();
            write!(reader.get_mut(), "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    (url, server)
}

#[tokio::test]
async fn checktx_rejection_can_be_recovered_at_the_same_sequence_after_restart() {
    let signer = key();
    let old = original(&signer, 7);
    let Transaction::Cosmos {
        hash: old_hash,
        sender,
        raw,
        ..
    } = &old
    else {
        unreachable!()
    };
    let directory = std::env::temp_dir().join(format!("axe-checktx-{}", rand::random::<u64>()));
    let session = Arc::new(
        session::Session::load(
            directory.join("journal.json"),
            B256::ZERO,
            example_plan(),
            "unused".into(),
        )
        .await
        .unwrap(),
    );
    session
        .record("AddRewards/cosmos".into(), old.clone())
        .await
        .unwrap();
    let account = json!({"account":{"account_number":"9","sequence":"7"}});
    let (lcd, server) = serve(
        vec![
            (format!("GET /cosmos/tx/v1beta1/txs/{old_hash}"), None),
            (
                format!("GET /cosmos/auth/v1beta1/accounts/{sender}"),
                Some(account.clone()),
            ),
            (
                "POST /cosmos/tx/v1beta1/txs".into(),
                Some(json!({"tx_response":{"code":13,"raw_log":"insufficient fees"}})),
            ),
        ],
        raw,
    );
    let error = session::scope(
        session.clone(),
        cosmos_recovery::resume(&lcd, &old, "AddRewards/cosmos"),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("--cosmos-fee"));
    assert!(session.journal.lock().await.confirmations.is_empty());
    server.join().unwrap();
    let new = cosmos_fees::resign(&old, &signer, "test-chain", 9, 300).unwrap();
    session::scope(
        session.clone(),
        journal::replace("AddRewards/cosmos", new.clone()),
    )
    .await
    .unwrap();
    // Simulate Ctrl+C after replacement persistence but before its first broadcast.
    let restored = reopen(&session).await;
    let Transaction::Cosmos { raw, hash, .. } = &new else {
        unreachable!()
    };
    let response = json!({"tx_response":{"code":0,"height":"13","txhash":hash}});
    let (lcd, server) = serve(
        vec![
            (format!("GET /cosmos/tx/v1beta1/txs/{old_hash}"), None),
            (format!("GET /cosmos/tx/v1beta1/txs/{hash}"), None),
            (
                format!("GET /cosmos/auth/v1beta1/accounts/{sender}"),
                Some(account),
            ),
            (
                "POST /cosmos/tx/v1beta1/txs".into(),
                Some(json!({"tx_response":{"code":0}})),
            ),
            (format!("GET /cosmos/tx/v1beta1/txs/{old_hash}"), None),
            (
                format!("GET /cosmos/tx/v1beta1/txs/{hash}"),
                Some(response.clone()),
            ),
        ],
        raw,
    );
    assert_eq!(
        session::scope(
            restored,
            cosmos_recovery::resume(&lcd, &new, "AddRewards/cosmos")
        )
        .await
        .unwrap(),
        response
    );
    server.join().unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}

async fn reopen(previous: &session::Session) -> Arc<session::Session> {
    Arc::new(
        session::Session::load(
            previous.path.clone(),
            B256::ZERO,
            example_plan(),
            "unused".into(),
        )
        .await
        .unwrap(),
    )
}
