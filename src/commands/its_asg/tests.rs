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
use super::types::{Artifact, Context, Options, Roles, SavedTransaction};
use super::{checks, config, handoff, signing, transactions};
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
        create3: Some(Address::repeat_byte(3)),
        configured_asg: None,
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
    for variable in [signing::SENDER_KEY_ENV, signing::OPERATOR_KEY_ENV] {
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
    let (url, server) = crate::evm::test_rpc::serve(vec![
        Some(json!({"result":receipt})),
        Some(json!({"result":block})),
        Some(json!({"result":receipt})),
    ])
    .await;
    let provider = crate::evm::connect_evm(&[url]).unwrap();
    let recovered = transactions::resume(&provider, &context(), &request, &saved, None)
        .await
        .unwrap();
    assert_eq!(recovered.transaction_hash, receipt.transaction_hash);
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        requests
            .iter()
            .all(|r| r["method"] != "eth_sendRawTransaction")
    );
}

#[tokio::test]
async fn consumed_nonce_blocks_recovery_before_any_broadcast() {
    let (request, saved) = signed_handoff();
    let mock = Asserter::new();
    mock.push_success(&serde_json::Value::Null);
    mock.push_success(&"0x8");
    let provider = ProviderBuilder::new().connect_client(RpcClient::mocked(mock));
    let error = transactions::resume(&provider, &context(), &request, &saved, None)
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
        artifact: Some(PathBuf::from("explicit-artifact.json")),
        apply: false,
        confirm_mainnet: false,
    };
    std::fs::write(&path, input.to_string()).unwrap();
    let loaded = config::load(Network::Testnet, "robinhood", &options)
        .await
        .unwrap();
    assert_eq!(
        loaded.artifact_path,
        PathBuf::from("explicit-artifact.json")
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
