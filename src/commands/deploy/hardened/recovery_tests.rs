use std::collections::BTreeMap;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::Arc;

use alloy::consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy::eips::Encodable2718;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, B256, Bytes, TxKind, U256};
use alloy::providers::ProviderBuilder;
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::signers::{SignerSync, local::PrivateKeySigner};
use alloy::transports::mock::Asserter;
use cosmos_sdk_proto::cosmos::tx::v1beta1::{AuthInfo, SignerInfo, TxRaw};
use prost::Message;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{
    journal, session, storage,
    tests::example_plan,
    types::{Confirmation, EvmConfirmationPolicy, ProtocolIdentity, Transaction},
};

pub(super) async fn session() -> Arc<session::Session> {
    let path = std::env::temp_dir()
        .join(format!(
            "axe-recovery-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ))
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

#[tokio::test]
async fn confirmed_cosmos_survives_restart_without_any_lcd_history() {
    let first = session().await;
    let raw = vec![1, 2, 3];
    let hash = hex::encode_upper(Sha256::digest(&raw));
    let response = json!({"tx_response":{"code":0,"height":"123","txhash":hash,"events":[]}});
    let saved = Transaction::Cosmos {
        raw,
        hash: hash.clone(),
        intent: B256::ZERO,
        sender: "test".into(),
        fee: "1".into(),
        sequence: 7,
    };
    first
        .record("AddRewards/cosmos".into(), saved.clone())
        .await
        .unwrap();
    session::scope(
        first.clone(),
        journal::confirm(
            hash,
            Confirmation::Cosmos {
                height: 123,
                code: 0,
                response: response.clone(),
            },
        ),
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
    let result = session::scope(
        restored,
        super::cosmos_recovery::resume("http://127.0.0.1:1", &saved, "AddRewards/cosmos"),
    )
    .await
    .unwrap();
    assert_eq!(result, response);
    std::fs::remove_dir_all(first.path.parent().unwrap()).unwrap();
}

#[test]
fn recorded_cosmos_sequence_must_match_signed_auth_info() {
    let auth = AuthInfo {
        signer_infos: vec![SignerInfo {
            sequence: 7,
            ..Default::default()
        }],
        ..Default::default()
    };
    let raw = TxRaw {
        auth_info_bytes: auth.encode_to_vec(),
        ..Default::default()
    }
    .encode_to_vec();
    super::cosmos_recovery::validate_sequence(&raw, 7).unwrap();
    assert!(super::cosmos_recovery::validate_sequence(&raw, 8).is_err());
}

#[test]
fn a_sequential_flow_reserves_one_deposit_and_recognizes_escrow() {
    assert_eq!(
        super::preflight::sequential_deposit(true, false, 400_000_000_000),
        400_000_000_000
    );
    assert_eq!(
        super::preflight::sequential_deposit(true, true, 400_000_000_000),
        0
    );
    assert_eq!(
        super::preflight::sequential_deposit(false, false, 400_000_000_000),
        0
    );
}

#[test]
fn hardened_mode_cannot_silently_fall_back_outside_its_task() {
    assert!(session::validate_context(true, false).is_err());
    session::validate_context(true, true).unwrap();
    session::validate_context(false, false).unwrap();
}

#[test]
fn canonical_intent_ignores_fee_changes_but_rejects_call_changes() {
    let original = TransactionRequest::default()
        .with_chain_id(1)
        .with_from(Address::from([1; 20]))
        .with_to(Address::from([2; 20]))
        .with_input(Bytes::from(vec![3]));
    let expected = super::evm_intent::hash(&original).unwrap();
    let mut changed = original.clone();
    changed.max_fee_per_gas = Some(400);
    assert_eq!(super::evm_intent::hash(&changed).unwrap(), expected);
    changed.value = Some(U256::from(1));
    assert_ne!(super::evm_intent::hash(&changed).unwrap(), expected);
    changed = original.with_nonce(8);
    assert_ne!(super::evm_intent::hash(&changed).unwrap(), expected);
}

fn signed(tx: TxEip1559, signer: &PrivateKeySigner) -> TxEnvelope {
    let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
    tx.into_signed(signature).into()
}

#[test]
fn replacement_keeps_execution_identical_and_both_hashes_differ() {
    let signer = PrivateKeySigner::random();
    let original = TxEip1559 {
        chain_id: 1,
        nonce: 7,
        gas_limit: 100_000,
        max_fee_per_gas: 10,
        max_priority_fee_per_gas: 1,
        to: TxKind::Call(Address::from([2; 20])),
        value: U256::from(123),
        input: Bytes::from(vec![4, 5, 6]),
        ..Default::default()
    };
    let old = signed(original.clone(), &signer);
    let mut changed = original;
    changed.max_fee_per_gas = 20;
    changed.max_priority_fee_per_gas = 2;
    let replacement = signed(changed.clone(), &signer);
    super::evm_recovery::validate_replacement(&old, &replacement).unwrap();
    assert_ne!(old.tx_hash(), replacement.tx_hash());
    assert_ne!(old.encoded_2718(), replacement.encoded_2718());
    for field in ["nonce", "gas", "value", "data", "destination", "chain"] {
        let mut invalid = changed.clone();
        match field {
            "nonce" => invalid.nonce += 1,
            "gas" => invalid.gas_limit += 1,
            "value" => invalid.value += U256::from(1),
            "data" => invalid.input = Bytes::from(vec![0]),
            "destination" => invalid.to = TxKind::Create,
            "chain" => invalid.chain_id += 1,
            _ => unreachable!(),
        }
        assert!(
            super::evm_recovery::validate_replacement(&old, &signed(invalid, &signer)).is_err(),
            "{field}"
        );
    }
    assert!(
        super::evm_recovery::validate_replacement(
            &old,
            &signed(changed, &PrivateKeySigner::random())
        )
        .is_err()
    );
}

#[test]
fn protocol_upgrade_approval_cannot_change_addresses_or_authorities() {
    let identity = ProtocolIdentity {
        address: "contract".into(),
        code_id: "1".into(),
        checksum: "old".into(),
        creator: "creator".into(),
        admin: "gov".into(),
    };
    let old = BTreeMap::from([("Router".into(), identity.clone())]);
    let mut upgraded = identity;
    upgraded.code_id = "2".into();
    upgraded.checksum = "new".into();
    super::protocols::validate_change(&old, &BTreeMap::from([("Router".into(), upgraded.clone())]))
        .unwrap();
    upgraded.admin = "another-admin".into();
    assert!(
        super::protocols::validate_change(&old, &BTreeMap::from([("Router".into(), upgraded)]))
            .is_err()
    );
}

#[cfg(unix)]
#[test]
fn config_writes_preserve_symlinks_and_permissions() {
    let dir = std::env::temp_dir().join(format!("axe-config-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("config.json");
    let link = dir.join("linked.json");
    std::fs::write(&target, b"old").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
    symlink(&target, &link).unwrap();
    storage::atomic_config_write(&link, b"new").unwrap();
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"new");
    assert_eq!(
        std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o640
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_spawned_task_does_not_inherit_the_required_session() {
    let session = session().await;
    session::scope(session.clone(), async {
        assert!(session::active());
        let result = tokio::spawn(async { session::validate_context(true, session::active()) })
            .await
            .unwrap();
        assert!(result.is_err());
    })
    .await;
    std::fs::remove_dir_all(session.path.parent().unwrap()).unwrap();
}

#[tokio::test(start_paused = true)]
async fn an_original_transaction_can_win_after_a_replacement_and_finality_can_lag() {
    let session = session().await;
    session.journal.lock().await.evm_confirmations = EvmConfirmationPolicy::Finalized;
    let sender = Address::repeat_byte(3);
    let original = Transaction::Evm {
        intent: B256::ZERO,
        raw: Bytes::from(vec![1]),
        hash: alloy::primitives::keccak256([1]),
        sender,
        nonce: 7,
        gas_cost: "1".into(),
    };
    let replacement = Transaction::Evm {
        intent: B256::ZERO,
        raw: Bytes::from(vec![2]),
        hash: alloy::primitives::keccak256([2]),
        sender,
        nonce: 7,
        gas_cost: "2".into(),
    };
    session
        .record("test/call".into(), original.clone())
        .await
        .unwrap();
    session::scope(
        session.clone(),
        journal::replace("test/call", replacement.clone()),
    )
    .await
    .unwrap();
    let restored = Arc::new(
        session::Session::load(
            session.path.clone(),
            B256::ZERO,
            example_plan(),
            session.rpc.clone(),
        )
        .await
        .unwrap(),
    );
    let hash = alloy::primitives::keccak256([1]);
    let receipt: TransactionReceipt = serde_json::from_value(json!({
        "type":"0x2", "status":"0x1", "cumulativeGasUsed":"0x5208", "logs":[],
        "logsBloom":format!("0x{}", "00".repeat(256)), "transactionHash":hash,
        "blockHash":B256::repeat_byte(2), "blockNumber":"0xa", "gasUsed":"0x5208",
        "effectiveGasPrice":"0x1", "from":sender, "to":null, "contractAddress":null
    }))
    .unwrap();
    let mock = Asserter::new();
    mock.push_success(&receipt); // original wins; no nonce query, bump or broadcast
    let mut block = alloy::rpc::types::Block::<alloy::rpc::types::Transaction>::default();
    block.header.inner.number = 9;
    mock.push_success(&block); // still waiting for finality
    mock.push_success(&receipt);
    block.header.inner.number = 10;
    block.header.hash = receipt.block_hash.unwrap();
    mock.push_success(&block);
    mock.push_success(&block);
    let provider = ProviderBuilder::new().connect_client(RpcClient::mocked(mock));
    let result = session::scope(
        restored.clone(),
        super::evm_recovery::resume(&provider, &replacement, B256::ZERO, "test/call"),
    )
    .await
    .unwrap();
    assert_eq!(result.transaction_hash, hash);
    assert!(
        restored
            .journal
            .lock()
            .await
            .confirmations
            .contains_key(&hash.to_string())
    );
    assert_eq!(restored.journal.lock().await.attempts["test/call"].len(), 1);
    std::fs::remove_dir_all(session.path.parent().unwrap()).unwrap();
}

#[tokio::test]
async fn pinned_initial_signers_do_not_depend_on_live_registry_membership() {
    let session = session().await;
    let initial = super::verification::snapshot(
        vec!["axelar1vykg4kxuanj87nsx7qllxuqxt2gk3g0lfgs29h".into()],
        vec![(Address::repeat_byte(1), 1)],
        1,
        B256::ZERO,
        "initial-set".into(),
    );
    session.journal.lock().await.initial_signers = Some(initial.clone());
    let state = super::tests::initial_state();
    let ctx = crate::commands::deploy::DeployContext {
        axelar_id: state.axelar_id.to_string(),
        rpc_url: "http://127.0.0.1:1".into(),
        target_json: std::path::PathBuf::from("/nonexistent-config"),
        state,
    };
    let observed = session::scope(session.clone(), super::verification::initial_set(&ctx))
        .await
        .unwrap();
    assert_eq!(observed.hash, initial.hash);
    session
        .journal
        .lock()
        .await
        .initial_signers
        .as_mut()
        .unwrap()
        .hash = B256::ZERO;
    assert!(
        session::scope(session.clone(), super::verification::initial_set(&ctx))
            .await
            .is_err()
    );
    std::fs::remove_dir_all(session.path.parent().unwrap()).unwrap();
}

#[tokio::test]
async fn unknown_and_successful_cosmos_outcomes_cannot_be_retried_as_new_payments() {
    let mut state = super::tests::initial_state();
    for step in &mut state.steps {
        if step.name == "AddRewards" {
            break;
        }
        step.status = crate::state::StepStatus::Completed;
    }
    let ctx = crate::commands::deploy::DeployContext {
        axelar_id: state.axelar_id.to_string(),
        rpc_url: state.rpc_url.clone(),
        target_json: state.target_json.clone(),
        state,
    };
    let mut session = session().await;
    Arc::get_mut(&mut session).unwrap().options.retry_failed = Some("AddRewards/cosmos".into());
    let raw = vec![1];
    let hash = hex::encode_upper(Sha256::digest(&raw));
    let saved = Transaction::Cosmos {
        raw,
        hash: hash.clone(),
        sender: "test".into(),
        sequence: 7,
        fee: "1".into(),
        intent: B256::ZERO,
    };
    session
        .record("AddRewards/cosmos".into(), saved.clone())
        .await
        .unwrap();
    session::scope(session.clone(), async {
        // Unreachable LCD: outcome cannot be established.
        assert!(
            super::recovery::retry_failed(&ctx, "AddRewards/cosmos", &saved, "http://127.0.0.1:1")
                .await
                .is_err()
        );
        let response = json!({"tx_response":{"code":0,"height":"123","txhash":hash}});
        journal::confirm(
            hash,
            Confirmation::Cosmos {
                height: 123,
                code: 0,
                response,
            },
        )
        .await
        .unwrap();
        assert!(
            super::recovery::retry_failed(&ctx, "AddRewards/cosmos", &saved, "http://127.0.0.1:1")
                .await
                .is_err()
        );
        assert!(session.get("AddRewards/cosmos").await.is_some());
        assert!(session.journal.lock().await.attempts.is_empty());
    })
    .await;
    std::fs::remove_dir_all(session.path.parent().unwrap()).unwrap();
}
