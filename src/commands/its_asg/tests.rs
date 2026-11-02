use std::path::PathBuf;

use alloy::consensus::{SignableTransaction, TxEnvelope, TxLegacy};
use alloy::eips::Encodable2718;
use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::providers::ProviderBuilder;
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::{Block, Transaction, TransactionReceipt, TransactionRequest};
use alloy::signers::{SignerSync, local::PrivateKeySigner};
use alloy::sol_types::{SolCall, SolValue};
use alloy::transports::mock::Asserter;
use clap::Parser;
use serde_json::json;

use super::contracts::Managed;
use super::types::{Artifact, Context, Options, Roles, SavedTransaction, TransactionAction};
use super::{checks, config, deploy, existing, factory, handoff, signing, transactions};
use crate::cli::{Cli, Commands};
use crate::commands::deploy::hardened::storage;
use crate::types::Network;

fn context() -> Context {
    Context {
        network: Network::Testnet,
        path: PathBuf::new(),
        original: Vec::new(),
        key: "robinhood".to_string().into(),
        chain_id: 46630,
        rpc: String::new(),
        gateway: Address::repeat_byte(1),
        its: Address::repeat_byte(2),
        configured_asg: None,
        create3: Some(Address::repeat_byte(9)),
        artifact_path: PathBuf::new(),
        settings: config::settings(Network::Testnet).unwrap(),
    }
}

fn roles(from: Address) -> Roles {
    Roles {
        gateway_owner: from,
        gateway_operator: Address::repeat_byte(4),
        its_owner: from,
        sender_is_its_operator: true,
        asg_is_its_operator: false,
    }
}

#[test]
fn network_policies_never_reuse_the_testnet_operator_on_mainnet() {
    let mainnet = config::settings(Network::Mainnet).unwrap();
    let testnet = config::settings(Network::Testnet).unwrap();
    assert_ne!(mainnet.operator, testnet.operator);
    assert_eq!(
        mainnet.operator.to_checksum(None),
        "0xc8a8399c3D1207f4e109673be7047604737c1D56"
    );
    assert_eq!(
        testnet.operator.to_checksum(None),
        "0x6e079CD1c6bBb72680DeDF2687d711AE9427eE8e"
    );
    assert_eq!((mainnet.delay, testnet.delay), (259_200, 300));
    assert!(config::settings(Network::Stagenet).is_err());
    assert!(config::settings(Network::DevnetAmplifier).is_err());
}

#[test]
fn deployment_requires_the_networks_established_sender_and_salt() {
    let testnet = config::settings(Network::Testnet).unwrap();
    let mainnet = config::settings(Network::Mainnet).unwrap();
    assert_eq!(
        testnet.deployer.to_checksum(None),
        "0x81e63eA8F64FEdB9858EB6E2176B431FBd10d1eC"
    );
    assert_eq!(
        mainnet.deployer.to_checksum(None),
        "0xba76c6980428A0b10CFC5d8ccb61949677A61233"
    );
    assert_eq!(testnet.salt, "v6.1.0");
    assert_eq!(mainnet.salt, "v6.0.4-axelar");
    assert_eq!(
        testnet.legacy_asg.to_checksum(None),
        "0xb55A09E9fde39755c39EF9A7911431Ba0c96332A"
    );
    assert_eq!(
        mainnet.legacy_asg.to_checksum(None),
        "0x7Acbae6CBa67d78AAf69e47000884aE00F9B2525"
    );
    for (selected, other) in [(&testnet, &mainnet), (&mainnet, &testnet)] {
        assert!(deploy::validate_inputs(selected, selected.deployer, None).is_ok());
        assert!(deploy::validate_inputs(selected, selected.deployer, Some(selected.salt)).is_ok());
        assert!(deploy::validate_inputs(selected, other.deployer, None).is_err());
        assert!(deploy::validate_inputs(selected, selected.deployer, Some(other.salt)).is_err());
    }
    assert_eq!(
        signing::key_variable(TransactionAction::Deploy(testnet.legacy_asg)),
        "ASG_DEPLOYER_PRIVATE_KEY"
    );
    assert_eq!(
        signing::key_variable(TransactionAction::Handoff(testnet.legacy_asg)),
        "ASG_PRIVATE_KEY"
    );
}

#[tokio::test]
async fn missing_configured_factory_stops_before_preparing_a_transaction() {
    let (url, server) = crate::evm::test_rpc::serve(vec![Some(json!({"result":"0x"}))]).await;
    let provider = crate::evm::connect_evm(&[url]).unwrap();
    let ctx = context();
    let Err(error) = deploy::prepare(&provider, &ctx, ctx.settings.deployer).await else {
        panic!("missing factory must prevent deployment");
    };
    assert!(error.to_string().contains("configured CREATE3 factory"));
    let calls = server.await.unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["method"], "eth_getCode");
    assert_eq!(
        serde_json::from_value::<Address>(calls[0]["params"][0].clone()).unwrap(),
        ctx.create3.unwrap()
    );
}

#[test]
fn constructor_arguments_are_the_network_policy_and_gateway() {
    let ctx = context();
    let artifact = Artifact {
        contract_name: "AxelarServiceGovernance".into(),
        bytecode: Bytes::from_static(&[0x60, 0x00]),
        deployed_bytecode: Bytes::from_static(&[0x00]),
    };
    let init = checks::creation(&ctx, &artifact);
    let args = <(Address, String, String, U256, Address)>::abi_decode_params(&init[2..]).unwrap();
    assert_eq!(
        args,
        (
            ctx.gateway,
            "axelar".into(),
            config::GOVERNANCE_ADDRESS.into(),
            U256::from(300),
            ctx.settings.operator
        )
    );
}

#[test]
fn handoff_payloads_only_transfer_ownership_and_have_no_native_value() {
    let ctx = context();
    let from = Address::repeat_byte(5);
    let asg = Address::repeat_byte(6);
    for target in [ctx.its, ctx.gateway] {
        let request = handoff::ownership_request(&ctx, from, target, asg);
        let call =
            Managed::transferOwnershipCall::abi_decode(request.input.input().unwrap()).unwrap();
        assert_eq!(call.newOwner, asg);
        assert_eq!(request.from, Some(from));
        assert_eq!(request.to, Some(target.into()));
        assert_eq!(request.value, Some(U256::ZERO));
        assert_eq!(request.chain_id, Some(46630));
    }
}

#[test]
fn handoff_refuses_wrong_authority_and_detects_operator_changes() {
    let from = Address::repeat_byte(5);
    let asg = Address::repeat_byte(6);
    let before = roles(from);
    assert!(handoff::validate_roles(&before, asg, from).is_ok());
    let mut after = roles(from);
    after.its_owner = asg;
    assert!(handoff::validate_roles(&after, asg, from).is_ok());
    assert!(handoff::operators_unchanged(&before, &after).is_ok());
    after.gateway_owner = Address::repeat_byte(7);
    assert!(handoff::validate_roles(&after, asg, from).is_err());
    after = roles(from);
    after.gateway_operator = asg;
    assert!(handoff::validate_roles(&after, asg, from).is_err());
    assert!(handoff::operators_unchanged(&before, &after).is_err());
    after = roles(from);
    after.asg_is_its_operator = true;
    assert!(handoff::validate_roles(&after, asg, from).is_err());
    assert!(handoff::operators_unchanged(&before, &after).is_err());
    after = roles(from);
    after.sender_is_its_operator = false;
    assert!(handoff::operators_unchanged(&before, &after).is_err());
}

#[test]
fn config_write_preserves_other_fields_and_rejects_concurrent_changes() {
    let mut ctx = context();
    ctx.path = std::env::temp_dir().join(format!("asg-config-test-{}.json", std::process::id()));
    ctx.original = serde_json::to_vec(&json!({
        "chains":{"robinhood":{"rpc":"keep", "contracts":{
            "InterchainTokenService":{"address":"keep", "version":"2.2.0", "owner":"old"}
        }}}, "axelar":{"keep":true}
    }))
    .unwrap();
    std::fs::write(&ctx.path, &ctx.original).unwrap();
    config::save(
        &mut ctx,
        &[("InterchainTokenService", json!({"owner":"new"}))],
    )
    .unwrap();
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ctx.path).unwrap()).unwrap();
    assert_eq!(
        saved["chains"]["robinhood"]["contracts"]["InterchainTokenService"],
        json!({"address":"keep", "version":"2.2.0", "owner":"new"})
    );
    assert_eq!(saved["axelar"], json!({"keep":true}));
    std::fs::write(&ctx.path, b"changed elsewhere").unwrap();
    assert!(config::save(&mut ctx, &[("AxelarGateway", json!({"owner":"new"}))]).is_err());
    assert_eq!(std::fs::read(&ctx.path).unwrap(), b"changed elsewhere");
    std::fs::remove_file(&ctx.path).unwrap();
}

#[test]
fn original_its_trust_cli_is_preserved_and_asg_requires_an_explicit_sender() {
    let cli = Cli::try_parse_from([
        "axe",
        "its-trust",
        "testnet",
        "robinhood",
        "--skip",
        "solana",
        "--apply",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::ItsTrust {
            action: None,
            apply: true,
            ..
        }
    ));
    for command in ["deploy-asg", "handoff-asg"] {
        assert!(Cli::try_parse_from(["axe", "its-trust", "mainnet", "arc", command]).is_err());
        let from = Address::repeat_byte(5).to_string();
        let cli = Cli::try_parse_from([
            "axe",
            "its-trust",
            "mainnet",
            "arc",
            command,
            "--from",
            &from,
            "--apply",
            "--confirm-mainnet",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Commands::ItsTrust {
                action: Some(_),
                apply: false,
                ..
            }
        ));
        assert!(
            Cli::try_parse_from([
                "axe",
                "its-trust",
                "mainnet",
                "arc",
                command,
                "--from",
                &from,
                "--yes"
            ])
            .is_err()
        );
    }
}

fn signed_handoff() -> (TransactionRequest, SavedTransaction) {
    let signer = PrivateKeySigner::random();
    let request = handoff::ownership_request(
        &context(),
        signer.address(),
        Address::repeat_byte(2),
        Address::repeat_byte(6),
    );
    let tx = TxLegacy {
        chain_id: request.chain_id,
        nonce: 7,
        gas_price: 1,
        gas_limit: 100_000,
        to: request.to.unwrap(),
        value: U256::ZERO,
        input: request.input.input().unwrap().clone(),
    };
    let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
    let envelope: TxEnvelope = tx.into_signed(signature).into();
    let mut prepared = request.clone();
    prepared.nonce = Some(7);
    prepared.gas = Some(100_000);
    prepared.gas_price = Some(1);
    prepared.transaction_type = Some(0);
    (
        request,
        SavedTransaction {
            request: prepared,
            raw: envelope.encoded_2718().into(),
        },
    )
}

#[test]
fn recovery_binds_signed_bytes_to_reviewed_sender_chain_payload_and_fees() {
    let (request, mut saved) = signed_handoff();
    assert!(transactions::validate(&request, &saved).is_ok());
    let mut changed = request.clone();
    changed.chain_id = Some(1);
    assert!(transactions::validate(&changed, &saved).is_err());
    changed = request.clone();
    changed.from = Some(Address::repeat_byte(9));
    assert!(transactions::validate(&changed, &saved).is_err());
    changed = request.clone();
    changed.input = Bytes::from_static(&[1, 2, 3]).into();
    assert_eq!(
        transactions::journal_path(&request).unwrap(),
        transactions::journal_path(&changed).unwrap()
    );
    assert!(transactions::validate(&changed, &saved).is_err());
    saved.request.gas_price = Some(2);
    assert!(transactions::validate(&request, &saved).is_err());
    saved.request.gas_price = Some(1);
    saved.request.to = Some(Address::repeat_byte(9).into());
    assert!(transactions::validate(&saved.request, &saved).is_err());
}

#[test]
fn sender_lock_excludes_other_targets_but_not_other_accounts_or_chains() {
    let (request, _) = signed_handoff();
    let from = request.from.unwrap();
    let chain = request.chain_id.unwrap();
    let mut other_target = request.clone();
    other_target.to = Some(Address::repeat_byte(9).into());
    assert_ne!(
        transactions::journal_path(&request).unwrap(),
        transactions::journal_path(&other_target).unwrap()
    );
    let identity = transactions::sender_lock_path(chain, from).unwrap();
    assert_ne!(
        identity,
        transactions::sender_lock_path(chain + 1, from).unwrap()
    );
    assert_ne!(
        identity,
        transactions::sender_lock_path(chain, Address::repeat_byte(9)).unwrap()
    );
    let path = std::env::temp_dir().join(identity.file_name().unwrap());
    let first = storage::lock(&path).unwrap();
    let competing_path = path.clone();
    assert!(
        std::thread::spawn(move || storage::lock(&competing_path).is_err())
            .join()
            .unwrap()
    );
    drop(first);
    drop(storage::lock(&path).unwrap());
    std::fs::remove_file(path).unwrap();
}

#[test]
fn signing_keys_are_explicit_and_bound_to_each_role_without_secret_errors() {
    let signer = PrivateKeySigner::random();
    let secret = signer.to_bytes().to_string();
    for variable in [
        signing::SENDER_KEY_ENV,
        signing::DEPLOYER_KEY_ENV,
        signing::OPERATOR_KEY_ENV,
    ] {
        assert!(signing::read_signer(variable, signer.address(), |_| None).is_err());
        assert!(signing::read_signer(variable, signer.address(), |_| Some(" ".into())).is_err());
        let error = signing::read_signer(variable, signer.address(), |_| Some("bad-key".into()))
            .unwrap_err()
            .to_string();
        assert!(error.contains(variable));
        assert!(!error.contains("bad-key"));
        let error = signing::read_signer(variable, Address::ZERO, |_| Some(secret.clone()))
            .unwrap_err()
            .to_string();
        assert!(!error.contains(&secret));
        assert_eq!(
            signing::read_signer(variable, signer.address(), |name| {
                assert_eq!(name, variable);
                Some(secret.clone())
            })
            .unwrap()
            .address(),
            signer.address()
        );
    }
}

#[tokio::test]
async fn mined_journal_recovers_without_signing_or_rebroadcasting() {
    let (request, saved) = signed_handoff();
    let tx = transactions::validate(&request, &saved).unwrap();
    let receipt: TransactionReceipt = serde_json::from_value(json!({
        "type":"0x0", "status":"0x1", "cumulativeGasUsed":"0x5208",
        "logsBloom":format!("0x{}", "00".repeat(256)), "logs":[],
        "transactionHash":tx.tx_hash(), "blockHash":B256::repeat_byte(2),
        "blockNumber":"0xa", "gasUsed":"0x5208", "effectiveGasPrice":"0x1",
        "from":request.from, "to":request.to, "contractAddress":null
    }))
    .unwrap();
    let mut block = Block::<Transaction>::default();
    block.header.inner.number = 10;
    block.header.hash = receipt.block_hash.unwrap();
    for action in [
        TransactionAction::Deploy(Address::repeat_byte(6)),
        TransactionAction::Handoff(Address::repeat_byte(6)),
    ] {
        let mut responses = vec![Some(json!({"result":receipt}))];
        match action {
            TransactionAction::Deploy(_) => responses.extend([
                Some(json!({"result":"0xb"})),
                Some(json!({"result":receipt})),
                Some(json!({"result":block})),
            ]),
            TransactionAction::Handoff(_) => responses.extend([
                Some(json!({"result":block})),
                Some(json!({"result":receipt})),
            ]),
        }
        let (url, server) = crate::evm::test_rpc::serve(responses).await;
        let provider = crate::evm::connect_evm(&[url]).unwrap();
        let recovered = transactions::resume(&provider, &context(), &request, &saved, action)
            .await
            .unwrap();
        assert_eq!(recovered.transaction_hash, receipt.transaction_hash);
        let requests = server.await.unwrap();
        match action {
            TransactionAction::Deploy(_) => assert_eq!(requests[1]["method"], "eth_blockNumber"),
            TransactionAction::Handoff(_) => assert_eq!(requests[1]["params"][0], "finalized"),
        }
        assert!(
            requests
                .iter()
                .all(|r| r["method"] != "eth_sendRawTransaction")
        );
    }
}

#[tokio::test]
async fn consumed_nonce_blocks_recovery_before_any_broadcast() {
    let (request, saved) = signed_handoff();
    let mock = Asserter::new();
    mock.push_success(&serde_json::Value::Null);
    mock.push_success(&"0x8");
    let provider = ProviderBuilder::new().connect_client(RpcClient::mocked(mock));
    let error = transactions::resume(
        &provider,
        &context(),
        &request,
        &saved,
        TransactionAction::Handoff(Address::repeat_byte(6)),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("nonce consumed"));
}

#[tokio::test]
async fn constructor_simulation_uses_creation_call_and_returns_immutable_runtime() {
    let (url, server) =
        crate::evm::test_rpc::serve(vec![Some(json!({"result":"0x60016002"}))]).await;
    let provider = crate::evm::connect_evm(&[url]).unwrap();
    let from = Address::repeat_byte(5);
    let runtime = checks::expected_runtime(&provider, Bytes::from_static(&[0x60, 0x00]), from)
        .await
        .unwrap();
    assert_eq!(runtime.as_ref(), &[0x60, 0x01, 0x60, 0x02]);
    let requests = server.await.unwrap();
    assert_eq!(requests[0]["method"], "eth_call");
    assert!(
        requests[0]["params"][0]
            .get("to")
            .is_none_or(serde_json::Value::is_null)
    );
    assert_eq!(requests[0]["params"][0]["from"], from.to_string());
}

#[tokio::test]
async fn different_asg_runtime_fails_before_reading_roles_or_sending() {
    let mock = Asserter::new();
    mock.push_success(&"0x6000");
    let provider = ProviderBuilder::new().connect_client(RpcClient::mocked(mock));
    let error = checks::governance(
        &provider,
        &context(),
        Address::repeat_byte(6),
        &Bytes::from_static(&[0x60, 0x01]),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("runtime mismatch"));
}

#[tokio::test]
async fn deployment_verifies_all_asg_properties_at_confirmed_state_without_finality() {
    let ctx = context();
    let runtime = Bytes::from_static(&[0x60, 0x01]);
    let mut block = Block::<Transaction>::default();
    block.header.inner.number = 10;
    block.header.hash = B256::repeat_byte(9);
    let encoded = [
        ctx.gateway.abi_encode(),
        ctx.settings.operator.abi_encode(),
        ctx.settings.operator.abi_encode(),
        U256::from(ctx.settings.delay).abi_encode(),
        config::GOVERNANCE_CHAIN.to_owned().abi_encode(),
        config::GOVERNANCE_ADDRESS.to_owned().abi_encode(),
        alloy::primitives::keccak256(config::GOVERNANCE_CHAIN).abi_encode(),
        alloy::primitives::keccak256(config::GOVERNANCE_ADDRESS).abi_encode(),
    ];
    let mut responses = vec![
        Some(json!({"result":"0xb"})),
        Some(json!({"result":block})),
        Some(json!({"result":runtime})),
    ];
    responses.extend(
        encoded
            .into_iter()
            .map(|value| Some(json!({"result":Bytes::from(value)}))),
    );
    responses.push(Some(json!({"result":block})));
    let (url, server) = crate::evm::test_rpc::serve(responses).await;
    let provider = crate::evm::connect_evm(&[url]).unwrap();
    checks::deployed_governance(&provider, &ctx, Address::repeat_byte(6), &runtime)
        .await
        .unwrap();
    let calls = server.await.unwrap();
    let pinned = json!({"blockHash":block.header.hash,"requireCanonical":true});
    for (index, call) in calls.iter().enumerate().take(11).skip(2) {
        if index == 5 {
            assert_eq!(call["params"][1], "latest");
        } else {
            assert_eq!(call["params"][1], pinned);
        }
    }
    assert_eq!(calls[11]["params"], json!(["0xa", false]));
}

#[tokio::test]
async fn config_rejects_wrong_network_and_legacy_gateways_before_rpc() {
    let path = std::env::temp_dir().join(format!("asg-input-test-{}.json", std::process::id()));
    let mut input = json!({"axelar":{"chainId":"axelar-testnet-lisbon-3"},"chains":{"robinhood":{
        "chainType":"evm","chainId":46630,"rpc":"http://127.0.0.1:1",
        "contracts":{
            "AxelarGateway":{"address":Address::repeat_byte(1),"connectionType":"amplifier"},
            "InterchainTokenService":{"address":Address::repeat_byte(2)}
        }
    }}});
    let options = Options {
        from: Address::repeat_byte(5),
        config: Some(path.clone()),
        rpc_url: Some("https://rpc.example/fake-token".into()),
        artifact: Some(PathBuf::from("explicit-artifact.json")),
        apply: false,
        confirm_mainnet: false,
    };
    std::fs::write(&path, input.to_string()).unwrap();
    let mut loaded = config::load(Network::Testnet, "robinhood", &options)
        .await
        .unwrap();
    assert_eq!(
        loaded.artifact_path,
        PathBuf::from("explicit-artifact.json")
    );
    assert_eq!(loaded.rpc, "https://rpc.example/fake-token");
    config::save(
        &mut loaded,
        &[(
            "AxelarServiceGovernance",
            json!({"address": Address::repeat_byte(7)}),
        )],
    )
    .unwrap();
    assert!(
        !std::fs::read_to_string(&path)
            .unwrap()
            .contains("fake-token")
    );
    assert!(
        config::load(Network::Mainnet, "robinhood", &options)
            .await
            .is_err()
    );
    input["chains"]["robinhood"]["contracts"]["AxelarGateway"]["connectionType"] =
        json!("consensus");
    std::fs::write(&path, input.to_string()).unwrap();
    assert!(
        config::load(Network::Testnet, "robinhood", &options)
            .await
            .is_err()
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn per_chain_prediction_is_allowed_but_never_replaces_configured_asg() {
    let mut ctx = context();
    let predicted = Address::repeat_byte(7);
    assert!(deploy::validate_prediction(&ctx, predicted).is_ok());
    assert!(deploy::validate_prediction(&ctx, Address::ZERO).is_err());
    ctx.configured_asg = Some(predicted);
    assert!(deploy::validate_prediction(&ctx, predicted).is_ok());
    assert!(deploy::validate_prediction(&ctx, ctx.settings.legacy_asg).is_err());
}

#[test]
fn factory_selection_uses_recorded_provenance_then_chain_config() {
    let chain_factory = Address::repeat_byte(9);
    let recorded_factory = Address::repeat_byte(8);
    let mut input = json!({"contracts":{"Create3Deployer":{"address":chain_factory}}});
    let cfg = serde_json::from_value::<crate::config::ChainConfig>(input.clone()).unwrap();
    assert_eq!(factory::configured(&cfg).unwrap(), Some(chain_factory));
    input["contracts"]["AxelarServiceGovernance"] = json!({"create3Deployer":recorded_factory});
    let cfg = serde_json::from_value::<crate::config::ChainConfig>(input.clone()).unwrap();
    assert_eq!(factory::configured(&cfg).unwrap(), Some(recorded_factory));
    for invalid in [json!("bad-address"), json!(Address::ZERO), json!(17)] {
        input["contracts"]["AxelarServiceGovernance"]["create3Deployer"] = invalid;
        let cfg = serde_json::from_value::<crate::config::ChainConfig>(input.clone()).unwrap();
        assert!(factory::configured(&cfg).is_err());
    }
    let cfg = serde_json::from_value::<crate::config::ChainConfig>(json!({})).unwrap();
    assert_eq!(factory::configured(&cfg).unwrap(), None);
    let mut ctx = context();
    ctx.create3 = None;
    assert!(factory::selected(&ctx).is_err());
}

#[tokio::test]
async fn configured_existing_asg_is_reused_without_querying_a_new_factory() {
    let mut ctx = context();
    ctx.configured_asg = Some(Address::repeat_byte(7));
    ctx.create3 = None;
    let (url, server) = crate::evm::test_rpc::serve(vec![Some(json!({"result":"0x6001"}))]).await;
    let provider = crate::evm::connect_evm(&[url]).unwrap();
    assert_eq!(
        existing::address(&provider, &ctx).await.unwrap(),
        ctx.configured_asg
    );
    let calls = server.await.unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        serde_json::from_value::<Address>(calls[0]["params"][0].clone()).unwrap(),
        ctx.configured_asg.unwrap()
    );
}

#[tokio::test]
async fn legacy_shared_asg_is_discovered_even_when_chain_factory_differs() {
    let ctx = context();
    let (url, server) = crate::evm::test_rpc::serve(vec![Some(json!({"result":"0x6001"}))]).await;
    let provider = crate::evm::connect_evm(&[url]).unwrap();
    assert_eq!(
        existing::address(&provider, &ctx).await.unwrap(),
        Some(ctx.settings.legacy_asg)
    );
    let calls = server.await.unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        serde_json::from_value::<Address>(calls[0]["params"][0].clone()).unwrap(),
        ctx.settings.legacy_asg
    );
}

#[tokio::test]
async fn configured_future_address_cannot_replace_existing_shared_asg() {
    let mut ctx = context();
    ctx.configured_asg = Some(Address::repeat_byte(7));
    let (url, server) = crate::evm::test_rpc::serve(vec![
        Some(json!({"result":"0x"})),
        Some(json!({"result":"0x6001"})),
    ])
    .await;
    let provider = crate::evm::connect_evm(&[url]).unwrap();
    assert!(existing::address(&provider, &ctx).await.is_err());
    server.await.unwrap();
}

fn deployment_artifacts() -> (PathBuf, Context) {
    let root = std::env::temp_dir().join(format!(
        "asg-factory-test-{}-{}",
        std::process::id(),
        PrivateKeySigner::random().address()
    ));
    let mut ctx = context();
    ctx.path = root.join("axelar-chains-config/info/testnet.json");
    let (factory_path, _) =
        crate::utils::artifact_paths_for_step("Create3Deployer", &root).unwrap();
    std::fs::create_dir_all(PathBuf::from(&factory_path).parent().unwrap()).unwrap();
    std::fs::write(factory_path, br#"{"bytecode":"0x6000"}"#).unwrap();
    ctx.artifact_path = root.join("asg.json");
    std::fs::write(&ctx.artifact_path, br#"{"contractName":"AxelarServiceGovernance","bytecode":"0x6000","deployedBytecode":"0x6002"}"#).unwrap();
    (root, ctx)
}

#[tokio::test]
async fn deployment_targets_selected_factory_and_accepts_its_prediction_on_both_networks() {
    let (root, mut ctx) = deployment_artifacts();
    for network in [Network::Testnet, Network::Mainnet] {
        ctx.settings = config::settings(network).unwrap();
        let predicted = Address::repeat_byte(7);
        let (url, server) = crate::evm::test_rpc::serve(vec![
            Some(json!({"result":"0x6001"})),
            Some(json!({"result":"0x6001"})),
            Some(json!({"result":"0x6001"})),
            Some(json!({"result":"0x6002"})),
            Some(json!({"result":Bytes::from(predicted.abi_encode())})),
        ])
        .await;
        let provider = crate::evm::connect_evm(&[url]).unwrap();
        let deployment = deploy::prepare(&provider, &ctx, ctx.settings.deployer)
            .await
            .unwrap();
        assert_eq!(deployment.address, predicted);
        assert_eq!(deployment.request.to, ctx.create3.map(Into::into));
        assert_eq!(deployment.request.from, Some(ctx.settings.deployer));
        let call = crate::evm::Create3Deployer::deployCall::abi_decode(
            deployment.request.input.input().unwrap(),
        )
        .unwrap();
        assert_eq!(call.salt, crate::evm::get_salt_from_key(ctx.settings.salt));
        let calls = server.await.unwrap();
        assert_eq!(calls.len(), 5);
        assert_eq!(calls[2]["params"][1], "finalized");
        assert_eq!(
            serde_json::from_value::<Address>(calls[4]["params"][0]["to"].clone()).unwrap(),
            ctx.create3.unwrap()
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn factory_runtime_mismatch_blocks_prediction_before_any_deployment() {
    let (root, ctx) = deployment_artifacts();
    for results in [vec!["0x6003", "0x6001"], vec!["0x6001", "0x6001", "0x6003"]] {
        let (url, server) = crate::evm::test_rpc::serve(
            results
                .into_iter()
                .map(|result| Some(json!({"result":result})))
                .collect(),
        )
        .await;
        let provider = crate::evm::connect_evm(&[url]).unwrap();
        assert!(
            deploy::prepare(&provider, &ctx, ctx.settings.deployer)
                .await
                .is_err()
        );
        let calls = server.await.unwrap();
        assert!(
            calls
                .iter()
                .all(|call| call["method"] != "eth_sendRawTransaction")
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}
