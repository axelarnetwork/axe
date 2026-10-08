use clap::Parser;

use super::tests::initial_state;
use crate::cli::{Cli, Commands, DeployCommands};
use crate::state::StepStatus;

#[test]
fn safe_deploy_is_the_default_on_every_network_without_an_opt_in_flag() {
    for network in ["mainnet", "testnet", "stagenet", "devnet-amplifier"] {
        let cli = Cli::try_parse_from(["axe", "--network", network, "deploy", "run"]).unwrap();
        let Commands::Deploy {
            subcommand: DeployCommands::Run {
                deployment_options, ..
            },
        } = cli.command
        else {
            panic!("expected deploy run");
        };
        assert_eq!(deployment_options.evm_wait_seconds, 1800);
    }
    assert!(Cli::try_parse_from(["axe", "deploy", "run", "--hardened"]).is_err());
}

#[test]
fn unsupported_state_is_rejected_regardless_of_progress() {
    let mut state = initial_state();
    super::loading::validate_supported_state(&state).unwrap();
    state.hardened_plan = None;
    for status in [StepStatus::Pending, StepStatus::Completed] {
        state.steps[0].status = status;
        let error = super::loading::validate_supported_state(&state).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported pre-journal deployment")
        );
    }
}

#[test]
fn legacy_execution_and_overrides_are_not_available() {
    assert!(Cli::try_parse_from(["axe", "deploy", "run", "--legacy"]).is_err());
    assert!(Cli::try_parse_from(["axe", "deploy", "reset"]).is_err());
    for flag in [
        "--private-key",
        "--artifact-path",
        "--proxy-artifact-path",
        "--salt",
    ] {
        assert!(Cli::try_parse_from(["axe", "deploy", "run", flag, "value"]).is_err());
    }
}

#[test]
fn cosmos_fee_recovery_requires_an_action_and_cannot_mix_with_retry() {
    let args = [
        "axe",
        "deploy",
        "run",
        "--bump-fees",
        "AddRewards/cosmos",
        "--cosmos-fee",
        "300000",
    ];
    assert!(Cli::try_parse_from(args).is_ok());
    assert!(Cli::try_parse_from(["axe", "deploy", "run", "--cosmos-fee", "300000"]).is_err());
    assert!(Cli::try_parse_from(args.into_iter().chain(["--legacy"])).is_err());
    assert!(
        Cli::try_parse_from(
            args.into_iter()
                .chain(["--retry-failed", "AddRewards/cosmos"])
        )
        .is_err()
    );
}

#[tokio::test]
async fn deployment_sender_requires_a_session_before_contacting_the_rpc() {
    let provider = alloy::providers::ProviderBuilder::new()
        .connect_http("http://127.0.0.1:1".parse().unwrap());
    let error = super::evm::send(
        &provider,
        alloy::rpc::types::TransactionRequest::default(),
        "missing session",
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("session"));
}

#[tokio::test]
async fn existing_state_can_be_read_and_smoke_cache_saved_without_enabling_deploy() {
    let directory = std::env::temp_dir().join(format!("axe-state-read-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("old-chain.json");
    let mut state = initial_state();
    state.hardened_plan = None;
    state.steps[0].status = StepStatus::Completed;
    crate::state::save_state_at(&state, &path).await.unwrap();
    let before = std::fs::read(&path).unwrap();
    let mut loaded = crate::state::loading::read_paths(vec![path.clone()], Some(state.env))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(loaded.hardened_plan.is_none());
    assert!(super::loading::validate_supported_state(&loaded).is_err());
    assert!(
        crate::state::deployment_state_path(&loaded)
            .unwrap()
            .ends_with(format!("{}.json", loaded.axelar_id))
    );
    loaded.sender_receiver_address = Some(alloy::primitives::Address::repeat_byte(9));
    loaded.mnemonic = "must-not-be-persisted".into();
    crate::state::save_state_at(&loaded, &path).await.unwrap();
    let cached = crate::state::loading::read_paths(vec![path.clone()], Some(state.env))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        cached.sender_receiver_address,
        loaded.sender_receiver_address
    );
    assert_eq!(cached.steps[0].status, StepStatus::Completed);
    assert!(cached.mnemonic.is_empty());
    assert!(cached.hardened_plan.is_none());
    assert!(
        crate::state::loading::read_paths(vec![path], Some(crate::types::Network::Mainnet))
            .await
            .unwrap()
            .is_none()
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn smoke_tests_need_only_their_own_two_credentials() {
    let mut state = initial_state();
    state.hardened_plan = None;
    state.deployer_private_key = None;
    state.mnemonic.clear();
    let error = crate::state::credentials::load_test_credentials(&mut state, |_| None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("DEPLOYER_PRIVATE_KEY"));
    assert!(error.contains("MNEMONIC"));
    assert!(error.contains(".env"));
    let key = alloy::signers::local::PrivateKeySigner::random()
        .to_bytes()
        .to_string();
    let mnemonic = bip32::Mnemonic::from_entropy([42; 32], bip32::Language::English)
        .phrase()
        .to_string();
    crate::state::credentials::load_test_credentials(&mut state, |name| match name {
        "EVM_PRIVATE_KEY" => Some(key.clone()),
        "MNEMONIC" => Some(mnemonic.clone()),
        unexpected => {
            assert_eq!(unexpected, "DEPLOYER_PRIVATE_KEY");
            None
        }
    })
    .unwrap();
    assert!(state.hardened_plan.is_none());
    assert!(state.gateway_deployer_private_key.is_none());
    // Existing credentials also work. No deployment variables are requested.
    crate::state::credentials::load_test_credentials(&mut state, |_| {
        panic!("unexpected env lookup")
    })
    .unwrap();
    state.mnemonic = "sensitive-invalid-mnemonic".into();
    let error = crate::state::credentials::load_test_credentials(&mut state, |_| None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("invalid smoke-test MNEMONIC"));
    assert!(!error.contains(&state.mnemonic));
    assert!(!error.contains(&key));
}

#[tokio::test]
async fn unused_legacy_file_does_not_hide_current_journal_but_progress_conflicts_do() {
    let session = super::recovery_tests::session().await;
    let directory = session.path.parent().unwrap();
    let current_path = directory.join("state.json");
    let legacy_path = directory.join("old-chain.json");
    let mut current = initial_state();
    current.hardened_fingerprint = Some(alloy::primitives::B256::ZERO);
    let mut legacy = current.clone();
    legacy.hardened_plan = None;
    legacy.hardened_fingerprint = None;
    legacy.predicted_gateway_address = None;
    legacy.sender_receiver_address = None;
    legacy.proposals.clear();
    for step in &mut legacy.steps {
        step.status = StepStatus::Pending;
    }
    crate::state::save_state_at(&current, &current_path)
        .await
        .unwrap();
    crate::state::save_state_at(&legacy, &legacy_path)
        .await
        .unwrap();
    let paths = vec![current_path.clone(), legacy_path.clone()];
    let loaded = crate::state::loading::read_paths(paths.clone(), Some(current.env))
        .await
        .unwrap()
        .unwrap();
    assert!(loaded.hardened_plan.is_some());
    assert!(legacy_path.exists());
    legacy.steps[0].status = StepStatus::Completed;
    crate::state::save_state_at(&legacy, &legacy_path)
        .await
        .unwrap();
    let error = crate::state::loading::read_paths(paths, Some(current.env))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains(&current_path.display().to_string()));
    assert!(error.contains(&legacy_path.display().to_string()));
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn retry_gas_requires_explicit_retry_and_votes_are_opt_in() {
    assert!(Cli::try_parse_from(["axe", "deploy", "run", "--retry-gas-limit", "200000"]).is_err());
    assert!(
        Cli::try_parse_from([
            "axe",
            "deploy",
            "run",
            "--retry-failed",
            "Operators/call",
            "--retry-gas-limit",
            "200000"
        ])
        .is_ok()
    );
    assert!(Cli::try_parse_from(["axe", "deploy", "status", "--votes"]).is_ok());
}
