use std::collections::BTreeMap;
use std::path::PathBuf;

use alloy::primitives::{Address, B256, Bytes};
use alloy::signers::local::PrivateKeySigner;
use serde_json::json;

use super::{
    plan,
    session::Session,
    storage,
    types::{Plan, Transaction},
};
use crate::state::{State, StepKind, default_steps, read_state_at, save_state_at};

fn directory() -> PathBuf {
    std::env::temp_dir().join(format!(
        "axe-hardened-test-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ))
}

pub(super) fn example_plan() -> Plan {
    Plan {
        evm_chain_id: 31337,
        axelar_chain_id: "axelar-testnet-lisbon-3".into(),
        gateway_owner: Address::from([1; 20]),
        operators_owner: Address::from([2; 20]),
        gas_service_owner: Address::from([3; 20]),
        its_owner: Address::from([4; 20]),
        factory_owner: Address::from([5; 20]),
        gateway_operator: Address::from([6; 20]),
        prover_admin: "axelar1vykg4kxuanj87nsx7qllxuqxt2gk3g0lfgs29h".into(),
        approved_verifiers: vec![],
        evm_gas_budget: "1000000000000000000".into(),
        cosmos_fee_budget: "1000000".into(),
        reward_amount: "1000000".into(),
        voting_threshold: [2, 3],
        signing_threshold: [2, 3],
        block_expiry: 50,
        confirmation_height: 1,
    }
}

#[test]
fn deployment_has_three_proposals_and_preserves_dependency_order() {
    let steps = plan::steps(&example_plan()).unwrap();
    let names: Vec<_> = steps.iter().map(|step| step.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "EvmCompatibilityCheck",
            "ConstAddressDeployer",
            "Create3Deployer",
            "PredictGatewayAddress",
            "AddCosmWasmConfig",
            "InstantiateChainContracts",
            "WaitInstantiateProposal",
            "SaveDeployedContracts",
            "RegisterDeployment",
            "WaitRegisterProposal",
            "AddRewards",
            "WaitForVerifierSet",
            "AxelarGateway",
            "Operators",
            "RegisterOperators",
            "AxelarGasService",
            "TransferOperatorsOwnership",
            "TransferGatewayOwnership",
            "TransferGasServiceOwnership",
            "DeployInterchainTokenService",
            "RegisterItsOnHub",
            "WaitItsHubRegistration"
        ],
        "deployment must not add on-chain actions or change dependency order"
    );
    let proposals: Vec<_> = steps
        .iter()
        .filter_map(|s| match &s.kind {
            StepKind::CosmosTx { proposal_key } if proposal_key != "addRewards" => {
                Some(proposal_key.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(proposals, ["instantiate", "register", "itsHubRegister"]);
    let position = |name| steps.iter().position(|step| step.name == name).unwrap();
    assert!(position("PredictGatewayAddress") < position("InstantiateChainContracts"));
    assert!(position("WaitForVerifierSet") < position("AxelarGateway"));
    assert!(position("AxelarGateway") < position("RegisterItsOnHub"));
}

#[test]
fn gateway_nonce_drift_cannot_change_the_address_registered_in_cosmos() {
    let sender = Address::from([1; 20]);
    let predicted = crate::evm::compute_create_address(sender, 43);
    super::verification::validate_gateway_prediction(sender, 42, predicted).unwrap();
    assert!(super::verification::validate_gateway_prediction(sender, 43, predicted).is_err());
    assert!(super::verification::validate_gateway_prediction(sender, u64::MAX, predicted).is_err());
}

#[test]
fn dangerous_plan_values_are_rejected() {
    let valid = example_plan();
    plan::validate(&valid).unwrap();
    let mut invalid = valid.clone();
    invalid.gateway_owner = Address::ZERO;
    assert!(plan::validate(&invalid).is_err());
    invalid = valid.clone();
    invalid.signing_threshold = [3, 2];
    assert!(plan::validate(&invalid).is_err());
    invalid = valid.clone();
    invalid.voting_threshold = [0, 0];
    assert!(plan::validate(&invalid).is_err());
    invalid = valid;
    invalid.reward_amount = "0".into();
    assert!(plan::validate(&invalid).is_err());
}

#[test]
fn public_plan_rejects_unknown_fields_including_credentials() {
    let mut value = serde_json::to_value(example_plan()).unwrap();
    value["mnemonic"] = json!("unexpected credential field");
    assert!(serde_json::from_value::<Plan>(value).is_err());
}

#[test]
fn lock_is_exclusive_and_released_when_process_handle_closes() {
    let root = directory();
    let path = root.join("run.lock");
    let first = storage::lock(&path).unwrap();
    assert!(storage::lock(&path).is_err());
    drop(first);
    drop(storage::lock(&path).unwrap());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn public_state_does_not_persist_credentials_and_round_trips() {
    let root = directory();
    let path = root.join("state.json");
    let key = PrivateKeySigner::random().to_bytes().to_string();
    let state: State = serde_json::from_value(json!({
        "axelarId":"test-chain", "rpcUrl":"http://127.0.0.1:8545", "targetJson":"testnet.json",
        "env":"testnet", "cosmSalt":"test", "mnemonic": "in-memory-only", "adminMnemonic":"in-memory-admin",
        "deployerPrivateKey":key,"gatewayDeployerPrivateKey":key,"gasServiceDeployerPrivateKey":key,"itsDeployerPrivateKey":key,
        "hardenedPlan":example_plan(),"steps":default_steps()
    })).unwrap();
    save_state_at(&state, &path).await.unwrap();
    let bytes = std::fs::read_to_string(&path).unwrap();
    assert!(!bytes.contains(&key));
    assert!(!bytes.contains("in-memory"));
    let restored = read_state_at(&path).await.unwrap();
    assert_eq!(restored.hardened_plan, state.hardened_plan);
    assert!(restored.mnemonic.is_empty());
    assert!(restored.deployer_private_key.is_none());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn durable_transaction_survives_restart_and_cannot_be_overwritten() {
    let root = directory();
    let path = root.join("journal.json");
    let fingerprint = B256::from([1; 32]);
    let first = Session::load(
        path.clone(),
        fingerprint,
        example_plan(),
        "http://localhost".into(),
    )
    .await
    .unwrap();
    let transaction = Transaction::Evm {
        intent: B256::ZERO,
        raw: Bytes::from(vec![42]),
        hash: B256::ZERO,
        sender: Address::from([9; 20]),
        nonce: 12,
        gas_cost: "100".into(),
    };
    first
        .record("deploy/contract".into(), transaction.clone())
        .await
        .unwrap();
    assert!(
        first
            .record("deploy/contract".into(), transaction)
            .await
            .is_err()
    );
    drop(first);
    let restored = Session::load(
        path.clone(),
        fingerprint,
        example_plan(),
        "http://localhost".into(),
    )
    .await
    .unwrap();
    assert!(matches!(
        restored.get("deploy/contract").await,
        Some(Transaction::Evm { nonce: 12, .. })
    ));
    assert!(
        Session::load(path, B256::ZERO, example_plan(), "http://localhost".into())
            .await
            .is_err()
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn interrupted_temporary_write_cannot_replace_committed_state() {
    let root = directory();
    let path = root.join("state.json");
    storage::atomic_write(&path, b"{\"committed\":true}").unwrap();
    std::fs::write(path.with_extension("interrupted.tmp"), b"{").unwrap();
    assert!(
        serde_json::from_slice::<BTreeMap<String, bool>>(&std::fs::read(&path).unwrap()).unwrap()["committed"]
    );
    storage::atomic_write(&path, b"{\"committed\":false}").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"{\"committed\":false}");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn interrupted_action_does_not_excuse_code_or_other_authority_changes() {
    let saved = super::types::ContractEvidence {
        address: Address::from([1; 20]),
        code_hash: B256::from([2; 32]),
        owner: Some(Address::from([3; 20])),
        implementation: Some(Address::from([4; 20])),
        operator: None,
        signer_hash: Some(B256::ZERO),
    };
    let mut changed = saved.clone();
    changed.owner = Some(Address::from([5; 20]));
    assert!(super::evidence::compatible(
        &saved,
        &changed,
        Some("TransferGatewayOwnership")
    ));
    changed.implementation = Some(Address::from([6; 20]));
    assert!(!super::evidence::compatible(
        &saved,
        &changed,
        Some("TransferGatewayOwnership")
    ));
}

#[test]
fn deployment_plan_does_not_require_a_verifier_roster() {
    let plan = example_plan();
    assert!(plan.approved_verifiers.is_empty());
    super::plan::validate(&plan).unwrap();
    assert!(
        serde_json::to_value(plan)
            .unwrap()
            .get("approvedVerifiers")
            .is_none()
    );
}

pub(super) fn initial_state() -> State {
    serde_json::from_value(json!({
        "axelarId":"example", "rpcUrl":"http://127.0.0.1:8545", "targetJson":"testnet.json",
        "env":"testnet", "cosmSalt":"test", "hardenedPlan":example_plan(),
        "steps":plan::steps(&example_plan()).unwrap()
    }))
    .unwrap()
}

#[test]
fn missing_journal_cannot_restart_a_previously_initialized_deployment() {
    let mut state = initial_state();
    let absent = directory().join("journal.json");
    super::runner::validate_journal_presence(&state, &absent).unwrap();
    state.hardened_fingerprint = Some(B256::ZERO);
    assert!(super::runner::validate_journal_presence(&state, &absent).is_err());
}

#[test]
fn changed_step_order_or_recipient_is_rejected() {
    let mut state = initial_state();
    plan::validate_steps(&state).unwrap();
    state.steps.swap(0, 1);
    assert!(plan::validate_steps(&state).is_err());
    state = initial_state();
    let transfer = state
        .steps
        .iter_mut()
        .find(|step| step.name == "TransferGatewayOwnership")
        .unwrap();
    if let StepKind::TransferOwnership { new_owner, .. } = &mut transfer.kind {
        *new_owner = Address::from([99; 20]);
    }
    assert!(plan::validate_steps(&state).is_err());
}

#[test]
fn activation_cannot_bypass_the_verifier_checkpoint() {
    let mut state = initial_state();
    assert!(super::runner::validate_activation(&state).is_err());
    for step in &mut state.steps {
        if step.name == "WaitForVerifierSet" {
            break;
        }
        step.status = crate::state::StepStatus::Completed;
    }
    super::runner::validate_activation(&state).unwrap();
}
