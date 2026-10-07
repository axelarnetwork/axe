use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use base64::Engine;
use reqwest::StatusCode;
use serde_json::json;

use super::{context, session};
use crate::commands::deploy::hardened::{session as scope, types::Paused, verifier_set, verifiers};
use crate::cosmos::CosmwasmQueryError;

const NOT_ENOUGH: &str = "not enough verifiers: query wasm contract failed";

#[test]
fn only_the_known_registry_failure_is_verifier_readiness() {
    for (status, body, expected) in [
        (
            500,
            json!({"code":2,"message":NOT_ENOUGH,"details":[]}).to_string(),
            true,
        ),
        (
            500,
            json!({"code":3,"message":NOT_ENOUGH}).to_string(),
            false,
        ),
        (
            403,
            json!({"code":2,"message":NOT_ENOUGH}).to_string(),
            false,
        ),
        (
            500,
            json!({"code":2,"message":"contract not found"}).to_string(),
            false,
        ),
        (500, NOT_ENOUGH.into(), false),
        (502, "upstream unavailable".into(), false),
    ] {
        let error = CosmwasmQueryError::Http {
            role: "primary",
            status: StatusCode::from_u16(status).unwrap(),
            body,
        };
        assert_eq!(verifier_set::insufficient_verifiers(&error), expected);
    }
    assert!(!verifier_set::insufficient_verifiers(
        &CosmwasmQueryError::Exhausted
    ));
}

fn registry_server(message: &str) -> (String, thread::JoinHandle<()>) {
    let failure = json!({"code":2,"message":message,"details":[]});
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        for (contract, query, status, body) in [
            (
                "prover",
                json!("current_verifier_set"),
                "200 OK",
                json!({"data":null}),
            ),
            (
                "registry",
                json!({"service":{"service_name":"amplifier","chain_name":"example"}}),
                "200 OK",
                json!({"data":{"min_num_verifiers":5,"max_num_verifiers":70}}),
            ),
            (
                "registry",
                json!({"active_verifiers":{"service_name":"amplifier","chain_name":"example"}}),
                "500 Internal Server Error",
                failure,
            ),
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 4096];
            let size = stream.read(&mut request).unwrap();
            let encoded = base64::engine::general_purpose::STANDARD.encode(query.to_string());
            let path =
                format!("GET /cosmwasm/wasm/v1/contract/{contract}/smart/{encoded} HTTP/1.1");
            assert!(String::from_utf8_lossy(&request[..size]).starts_with(&path));
            let body = body.to_string();
            write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    (url, server)
}

#[tokio::test]
async fn initialization_pauses_without_writes_only_when_verifiers_are_not_ready() {
    for (message, should_pause) in [(NOT_ENOUGH, true), ("contract not found", false)] {
        let session = session().await;
        let before = std::fs::read(&session.path).unwrap();
        let (lcd, server) = registry_server(message);
        let path = session.path.parent().unwrap().join("config.json");
        let config = json!({"chains":{},"axelar":{"lcd":lcd,"chainId":"axelar-testnet-lisbon-3","gasPrice":"0.007uaxl",
            "contracts":{"ServiceRegistry":{"address":"registry"},"Multisig":{"address":"multisig"},
                "MultisigProver":{"example":{"address":"prover"}},"VotingVerifier":{"example":{"address":"verifier"}}}}});
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        let mut ctx = context();
        ctx.state.target_json = path.clone();
        ctx.target_json = path;
        // No signing credentials: readiness must stop initialization before signing.
        ctx.state.admin_mnemonic = None;
        let error = scope::scope(session.clone(), verifiers::initialize(&mut ctx))
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<Paused>().is_some(),
            should_pause,
            "{error:?}"
        );
        if should_pause {
            assert!(error.to_string().contains("resume with --activate"));
            assert!(
                error
                    .to_string()
                    .contains("No verifier initialization transaction was submitted")
            );
        } else {
            assert!(format!("{error:?}").contains(message));
        }
        assert_eq!(std::fs::read(&session.path).unwrap(), before);
        assert!(session.journal.lock().await.actions.is_empty());
        assert!(session.journal.lock().await.initial_signers.is_none());
        server.join().unwrap();
        std::fs::remove_dir_all(session.path.parent().unwrap()).unwrap();
    }
}
