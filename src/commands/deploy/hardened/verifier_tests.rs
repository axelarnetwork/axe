use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;

use alloy::primitives::{Address, B256, Bytes};
use base64::Engine;
use cosmrs::crypto::secp256k1::SigningKey;
use serde_json::{Value, json};

use super::{
    session,
    tests::{example_plan, initial_state},
    types::{InitialSigners, Service, Transaction},
    verification, verifier_set,
};
use crate::{commands::deploy::DeployContext, types::Network};

mod readiness;

const MAINNET_STAKIN: &str = "axelar15k8d4hqgytdxmcx3lhph2qagvt0r7683cchglj";
const TESTNET_STAKIN: &str = "axelar1j3u6kd4027wln9vnvmg449hmc3xj2m2g5uh69q";
const UNKNOWN: &str = "axelar1vykg4kxuanj87nsx7qllxuqxt2gk3g0lfgs29h";

fn response(count: usize) -> Value {
    let mut signers = BTreeMap::new();
    for (index, address) in [MAINNET_STAKIN, TESTNET_STAKIN, UNKNOWN]
        .into_iter()
        .take(count)
        .enumerate()
    {
        let key = SigningKey::from_slice(&[index as u8 + 1; 32]).unwrap();
        signers.insert(address, json!({"address":address, "weight":"1", "pub_key":{"ecdsa":hex::encode(key.public_key().to_bytes())}}));
    }
    json!({"id":"set-id", "verifier_set":{"signers":signers,"threshold":(count as u128 * 2).div_ceil(3).to_string(),"created_at":42}})
}

fn limits(minimum: u64) -> Service {
    Service {
        min_num_verifiers: minimum,
        max_num_verifiers: None,
    }
}

fn validate(response: Value, service: &Service) -> eyre::Result<InitialSigners> {
    verifier_set::validate(serde_json::from_value(response)?, service, [2, 3])
}

#[test]
fn enough_actual_prover_signers_pass_without_a_predetermined_roster() {
    for (count, minimum, accepted) in [(1, 2, false), (2, 2, true), (3, 2, true)] {
        assert_eq!(
            validate(response(count), &limits(minimum)).is_ok(),
            accepted
        );
    }
    let service = Service {
        min_num_verifiers: 2,
        max_num_verifiers: Some(2),
    };
    assert!(validate(response(3), &service).is_err());
    assert!(validate(response(1), &limits(0)).is_err());
    let service = Service {
        min_num_verifiers: 3,
        max_num_verifiers: Some(2),
    };
    assert!(validate(response(3), &service).is_err());
}

#[test]
fn name_lookup_is_network_specific_and_unknown_names_do_not_block() {
    assert_eq!(
        verifier_set::name(Network::Mainnet, MAINNET_STAKIN),
        "Stakin"
    );
    assert_eq!(
        verifier_set::name(Network::Testnet, TESTNET_STAKIN),
        "Stakin"
    );
    for (network, address) in [
        (Network::Mainnet, TESTNET_STAKIN),
        (Network::Mainnet, UNKNOWN),
        (Network::DevnetAmplifier, MAINNET_STAKIN),
    ] {
        assert!(verifier_set::name(network, address).starts_with("Unknown"));
    }
    let initial = validate(response(3), &limits(2)).unwrap();
    assert!(initial.identities.iter().any(|address| address == UNKNOWN));
    assert_eq!(initial.threshold, 2);
    assert!(initial.signers.windows(2).all(|pair| pair[0].0 < pair[1].0));
    let fixture = response(3);
    for (identity, (key, weight)) in initial.identities.iter().zip(&initial.signers) {
        let hex = fixture["verifier_set"]["signers"][identity]["pub_key"]["ecdsa"]
            .as_str()
            .unwrap();
        assert_eq!(
            *key,
            crate::evm::pubkey_to_address(&hex::decode(hex).unwrap()).unwrap()
        );
        assert_eq!(*weight, 1);
    }
}

#[test]
fn invalid_keys_identities_weights_and_thresholds_fail_closed() {
    for (pointer, value) in [
        (
            format!("/verifier_set/signers/{UNKNOWN}/pub_key/ecdsa"),
            json!("bad-key"),
        ),
        (
            format!("/verifier_set/signers/{UNKNOWN}/address"),
            json!(MAINNET_STAKIN),
        ),
        (
            format!("/verifier_set/signers/{UNKNOWN}/weight"),
            json!("0"),
        ),
        (
            format!("/verifier_set/signers/{UNKNOWN}/weight"),
            json!(u128::MAX.to_string()),
        ),
        ("/verifier_set/threshold".into(), json!("1")),
        ("/verifier_set/threshold".into(), json!("0")),
        ("/id".into(), json!("")),
    ] {
        let mut set = response(3);
        *set.pointer_mut(&pointer).unwrap() = value;
        assert!(validate(set, &limits(2)).is_err(), "{pointer}");
    }
    let mut duplicate = response(3);
    duplicate["verifier_set"]["signers"][UNKNOWN]["pub_key"] =
        duplicate["verifier_set"]["signers"][MAINNET_STAKIN]["pub_key"].clone();
    assert!(
        validate(duplicate, &limits(2))
            .unwrap_err()
            .to_string()
            .contains("duplicate signer")
    );
    let mut missing = response(3);
    missing["verifier_set"]["signers"][UNKNOWN]
        .as_object_mut()
        .unwrap()
        .remove("pub_key");
    assert!(validate(missing, &limits(2)).is_err());
}

#[test]
fn weighted_threshold_uses_ceiling_and_comparison_detects_set_changes() {
    let mut set = response(3);
    set["verifier_set"]["signers"][UNKNOWN]["weight"] = json!("2");
    set["verifier_set"]["threshold"] = json!(3);
    let first = validate(set.clone(), &limits(2)).unwrap();
    assert_eq!(first.threshold, 3);
    let same = validate(set.clone(), &limits(2)).unwrap();
    assert!(verifier_set::same_set(&first, &same));
    set["verifier_set"]["created_at"] = json!(43);
    assert!(!verifier_set::same_set(
        &first,
        &validate(set, &limits(2)).unwrap()
    ));
    assert!(!verifier_set::same_set(
        &first,
        &validate(response(2), &limits(2)).unwrap()
    ));
}

async fn session() -> Arc<session::Session> {
    let path = std::env::temp_dir()
        .join(format!("axe-verifiers-{}", rand::random::<u64>()))
        .join("journal.json");
    Arc::new(
        session::Session::load(
            path,
            B256::ZERO,
            example_plan(),
            "http://127.0.0.1:1".into(),
        )
        .await
        .unwrap(),
    )
}

fn context() -> DeployContext {
    let state = initial_state();
    DeployContext {
        axelar_id: state.axelar_id.to_string(),
        rpc_url: state.rpc_url.clone(),
        target_json: state.target_json.clone(),
        state,
    }
}

#[tokio::test]
async fn approval_is_required_and_saved_set_survives_restart_without_live_membership() {
    let first = session().await;
    let ctx = context();
    assert!(
        session::scope(first.clone(), verification::initial_set(&ctx))
            .await
            .unwrap_err()
            .to_string()
            .contains("not been approved")
    );
    let initial = validate(response(3), &limits(2)).unwrap();
    session::scope(
        first.clone(),
        verifier_set::save_decision(initial.clone(), true),
    )
    .await
    .unwrap();
    let restored = Arc::new(
        session::Session::load(
            first.path.clone(),
            B256::ZERO,
            example_plan(),
            first.rpc.clone(),
        )
        .await
        .unwrap(),
    );
    let pinned = session::scope(restored.clone(), verification::initial_set(&ctx))
        .await
        .unwrap();
    assert!(verifier_set::same_set(&initial, &pinned));
    // Completed initialization never queries the registry or submits another update.
    session::scope(restored, super::verifiers::initialize(&mut context()))
        .await
        .unwrap();
    std::fs::remove_dir_all(first.path.parent().unwrap()).unwrap();
}

#[tokio::test]
async fn signed_gateway_proxy_makes_the_initial_snapshot_immutable() {
    let session = session().await;
    let initial = validate(response(3), &limits(2)).unwrap();
    session::scope(
        session.clone(),
        verifier_set::save_decision(initial.clone(), true),
    )
    .await
    .unwrap();
    session
        .record(
            "AxelarGateway/gateway proxy".into(),
            Transaction::Evm {
                intent: B256::ZERO,
                raw: Bytes::from(vec![1]),
                hash: B256::ZERO,
                sender: Address::ZERO,
                nonce: 4,
                gas_cost: "1".into(),
            },
        )
        .await
        .unwrap();
    let before = std::fs::read(&session.path).unwrap();
    let changed = validate(response(2), &limits(2)).unwrap();
    let error = session::scope(session.clone(), verifier_set::save_decision(changed, true))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("already journaled"));
    assert_eq!(std::fs::read(&session.path).unwrap(), before);
    // Recovery of signed bytes must not query a now-different or unavailable prover.
    session::scope(
        session.clone(),
        verification::refresh_before_gateway(&context()),
    )
    .await
    .unwrap();
    std::fs::remove_dir_all(session.path.parent().unwrap()).unwrap();
}

#[tokio::test]
async fn declined_set_preserves_pending_and_previously_approved_journals() {
    let session = session().await;
    let initial = validate(response(3), &limits(2)).unwrap();
    for previously_approved in [false, true] {
        if previously_approved {
            session::scope(
                session.clone(),
                verifier_set::save_decision(initial.clone(), true),
            )
            .await
            .unwrap();
        }
        let before = std::fs::read(&session.path).unwrap();
        let changed = validate(response(2), &limits(2)).unwrap();
        let error = session::scope(session.clone(), verifier_set::save_decision(changed, false))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("declined"));
        assert_eq!(std::fs::read(&session.path).unwrap(), before);
        assert_eq!(
            session.journal.lock().await.initial_signers.is_some(),
            previously_approved
        );
    }
    std::fs::remove_dir_all(session.path.parent().unwrap()).unwrap();
}

fn prover_server(value: Value) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        for (contract, query, data) in [
            ("prover", json!("current_verifier_set"), value),
            (
                "registry",
                json!({"service":{"service_name":"amplifier","chain_name":"example"}}),
                json!({"min_num_verifiers":2,"max_num_verifiers":null}),
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
            let body = json!({"data":data}).to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    (url, server)
}

#[tokio::test]
async fn pre_proxy_check_reads_the_actual_prover_set_without_requiring_current_registry_roster() {
    let session = session().await;
    let initial = validate(response(3), &limits(2)).unwrap();
    session::scope(session.clone(), verifier_set::save_decision(initial, true))
        .await
        .unwrap();
    let (lcd, server) = prover_server(response(3));
    let path = session.path.parent().unwrap().join("config.json");
    let config = json!({"chains":{},"axelar":{"lcd":lcd,"chainId":"axelar-testnet-lisbon-3","gasPrice":"0.007uaxl",
        "contracts":{"MultisigProver":{"example":{"address":"prover"}},"ServiceRegistry":{"address":"registry"}}}});
    std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    let mut ctx = context();
    ctx.state.target_json = path.clone();
    ctx.target_json = path;
    // The server only accepts current_verifier_set and service queries, never active_verifiers.
    session::scope(session.clone(), verification::refresh_before_gateway(&ctx))
        .await
        .unwrap();
    server.join().unwrap();
    std::fs::remove_dir_all(session.path.parent().unwrap()).unwrap();
}
