use super::{inputs, recovery_tests::session, storage, tests::initial_state};
use alloy::primitives::{B256, keccak256};
use serde_json::json;

#[tokio::test]
async fn pinned_inputs_survive_source_removal_and_rebuild_the_artifact_cache() {
    let session = session().await;
    let directory = session.path.parent().unwrap().to_path_buf();
    let source = directory.join("source");
    let mut state = initial_state();
    state.target_json = source.join("axelar-chains-config/info/testnet.json");
    let artifact =
        json!({"abi":[],"bytecode":"0x6000","deployedBytecode":"0x00", "metadata":"not needed"});
    for path in inputs::artifact_paths(&source).unwrap() {
        storage::atomic_write(&path, &serde_json::to_vec(&artifact).unwrap()).unwrap();
    }
    let config = json!({"axelar":{"contracts":{
        "Gateway":{"storeCodeProposalCodeHash":"gateway-hash"},
        "VotingVerifier":{"storeCodeProposalCodeHash":"verifier-hash"},
        "MultisigProver":{"storeCodeProposalCodeHash":"prover-hash"}
    }}});
    storage::atomic_write(&state.target_json, &serde_json::to_vec(&config).unwrap()).unwrap();
    let prepared = inputs::prepare_at(&state, directory.clone()).await.unwrap();
    assert!(
        prepared
            .snapshot
            .artifacts
            .values()
            .all(|artifact| artifact.get("metadata").is_none())
    );
    inputs::scope(
        prepared,
        inputs::commit(&session, json!({"plan":"original"})),
    )
    .await
    .unwrap();
    assert!(session.journal.lock().await.inputs_hash.is_some());
    std::fs::remove_dir_all(&source).unwrap();
    std::fs::remove_dir_all(directory.join("artifacts")).unwrap();
    let restored = inputs::prepare_at(&state, directory.clone()).await.unwrap();
    inputs::scope(restored, async {
        assert_eq!(
            inputs::cosmos_code_hash(&state.target_json, "Gateway")
                .await
                .unwrap(),
            "gateway-hash"
        );
        for path in inputs::artifact_paths(&inputs::root(&state).unwrap()).unwrap() {
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            assert_eq!(value["bytecode"], "0x6000");
        }
        inputs::check_fingerprint(&json!({"plan":"original"})).unwrap();
        let error = inputs::check_fingerprint(&json!({"plan":"changed"})).unwrap_err();
        assert!(error.to_string().contains("plan"));
    })
    .await;
    std::fs::write(directory.join("inputs.json"), b"{}").unwrap();
    assert!(inputs::prepare_at(&state, directory.clone()).await.is_err());
    std::fs::remove_file(directory.join("inputs.json")).unwrap();
    assert!(inputs::prepare_at(&state, directory.clone()).await.is_err());
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn input_snapshot_rejects_tampering_and_paths_outside_its_cache() {
    let session = session().await;
    let directory = session.path.parent().unwrap().to_path_buf();
    let bytes = serde_json::to_vec(&json!({"version":1,"artifacts":{"../escape":{}},"cosmos_codes":{},"fingerprint_inputs":null})).unwrap();
    assert!(inputs::decode(&bytes, B256::ZERO).is_err());
    let snapshot = inputs::decode(&bytes, keccak256(&bytes)).unwrap();
    assert!(
        inputs::materialize(&super::input_types::PreparedInputs {
            snapshot,
            directory: directory.clone()
        })
        .is_err()
    );
    assert!(!directory.join("escape").exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn old_journal_fingerprint_mismatch_cannot_commit_new_inputs() {
    let session = session().await;
    let before = std::fs::read(&session.path).unwrap();
    assert!(
        super::session::Session::load(
            session.path.clone(),
            B256::repeat_byte(9),
            session.plan.clone(),
            session.rpc.clone()
        )
        .await
        .is_err()
    );
    assert_eq!(std::fs::read(&session.path).unwrap(), before);
    assert!(session.journal.lock().await.inputs_hash.is_none());
    std::fs::remove_dir_all(session.path.parent().unwrap()).unwrap();
}
