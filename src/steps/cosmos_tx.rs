use std::fs;

use base64::Engine;
use cosmos_sdk_proto::cosmos::base::v1beta1::Coin as ProtoCoin;
use eyre::Result;
use serde_json::{Value, json};

use crate::commands::deploy::DeployContext;
use crate::cosmos::{
    build_execute_msg_any, build_execute_msg_any_with_funds, build_submit_proposal_any,
    derive_axelar_wallet, extract_proposal_id, lcd_fetch_code_id, read_axelar_config,
    read_axelar_contract_field, sign_and_broadcast_cosmos_tx,
};
use crate::evm::get_salt_from_key;
use crate::state::Step;
use crate::types::Network;
use crate::ui;
use crate::utils::compute_domain_separator;

/// Default deposit (in `uaxl`) attached to a cosmos governance proposal.
/// Falls back to this when `.env` doesn't override `PROPOSAL_DEPOSIT`.
const DEFAULT_PROPOSAL_DEPOSIT_UAXL: &str = "3000000000";

/// Multisig proposal `reward_amount` per signer, in `uaxl`.
const DEFAULT_REWARD_AMOUNT_UAXL: &str = "1000000";

/// Default `block_expiry` for the VotingVerifier when the chain config
/// doesn't supply one. 50 blocks ≈ poll-window default Axelar advertises.
const DEFAULT_VV_BLOCK_EXPIRY: u64 = 50;

pub async fn run(ctx: &mut DeployContext, step: &Step, step_name: &str) -> Result<()> {
    let mnemonic = ctx.state.mnemonic.clone();
    let env = ctx.state.env;
    let (signing_key, axelar_address) = derive_axelar_wallet(&mnemonic)?;
    let (lcd, chain_id, fee_denom, gas_price) = read_axelar_config(&ctx.target_json)?;
    let use_governance = env != Network::DevnetAmplifier;

    let chain_axelar_id = {
        let content = fs::read_to_string(&ctx.target_json)?;
        let root: Value = serde_json::from_str(&content)?;
        root.pointer(&format!("/chains/{}/axelarId", ctx.axelar_id))
            .and_then(|v| v.as_str())
            .unwrap_or(&ctx.axelar_id)
            .to_string()
    };

    let proposal_key = step.proposal_key().unwrap_or("").to_string();

    match step_name {
        "InstantiateChainContracts" => {
            run_instantiate(
                ctx,
                &signing_key,
                &axelar_address,
                &lcd,
                &chain_id,
                &fee_denom,
                gas_price,
                use_governance,
                &chain_axelar_id,
                env.as_str(),
                &proposal_key,
            )
            .await?;
        }
        "RegisterDeployment" => {
            run_register_deployment(
                ctx,
                &signing_key,
                &axelar_address,
                &lcd,
                &chain_id,
                &fee_denom,
                gas_price,
                use_governance,
                &chain_axelar_id,
                env.as_str(),
                &proposal_key,
            )
            .await?;
        }
        "CreateRewardPools" => {
            run_create_reward_pools(
                ctx,
                &signing_key,
                &axelar_address,
                &lcd,
                &chain_id,
                &fee_denom,
                gas_price,
                use_governance,
                &chain_axelar_id,
                env.as_str(),
                &proposal_key,
            )
            .await?;
        }
        "AddRewards" => {
            run_add_rewards(
                ctx,
                &signing_key,
                &axelar_address,
                &lcd,
                &chain_id,
                &fee_denom,
                gas_price,
                &chain_axelar_id,
            )
            .await?;
        }
        "RegisterItsOnHub" => {
            run_register_its_on_hub(
                ctx,
                &signing_key,
                &axelar_address,
                &lcd,
                &chain_id,
                &fee_denom,
                gas_price,
                use_governance,
                &chain_axelar_id,
                env.as_str(),
                &proposal_key,
            )
            .await?;
        }
        _ => {
            return Err(eyre::eyre!("unknown cosmos-tx step: {step_name}"));
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_instantiate(
    ctx: &mut DeployContext,
    signing_key: &cosmrs::crypto::secp256k1::SigningKey,
    axelar_address: &str,
    lcd: &str,
    chain_id: &str,
    fee_denom: &str,
    gas_price: f64,
    use_governance: bool,
    chain_axelar_id: &str,
    env: &str,
    proposal_key: &str,
) -> Result<()> {
    ui::info(&format!(
        "instantiating chain contracts for {chain_axelar_id}..."
    ));

    let coordinator_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Coordinator/address")?;
    let rewards_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Rewards/address")?;
    let multisig_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Multisig/address")?;
    let _router_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Router/address")?;
    let chain_codec_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/ChainCodecEvm/address")?;
    let governance_address =
        read_axelar_contract_field(&ctx.target_json, "/axelar/governanceAddress")?;

    ui::info("fetching code IDs...");
    let gateway_hash = read_axelar_contract_field(
        &ctx.target_json,
        "/axelar/contracts/Gateway/storeCodeProposalCodeHash",
    )?;
    let verifier_hash = read_axelar_contract_field(
        &ctx.target_json,
        "/axelar/contracts/VotingVerifier/storeCodeProposalCodeHash",
    )?;
    let prover_hash = read_axelar_contract_field(
        &ctx.target_json,
        "/axelar/contracts/MultisigProver/storeCodeProposalCodeHash",
    )?;

    let gateway_code_id = lcd_fetch_code_id(lcd, &gateway_hash).await?;
    let verifier_code_id = lcd_fetch_code_id(lcd, &verifier_hash).await?;
    let prover_code_id = lcd_fetch_code_id(lcd, &prover_hash).await?;
    ui::kv(
        "code IDs",
        &format!("gateway={gateway_code_id}, verifier={verifier_code_id}, prover={prover_code_id}"),
    );

    let content = fs::read_to_string(&ctx.target_json)?;
    let root: Value = serde_json::from_str(&content)?;
    let vv_config = root
        .pointer(&format!(
            "/axelar/contracts/VotingVerifier/{chain_axelar_id}"
        ))
        .ok_or_else(|| eyre::eyre!("no VotingVerifier.{chain_axelar_id} config"))?;
    let mp_config = root
        .pointer(&format!(
            "/axelar/contracts/MultisigProver/{chain_axelar_id}"
        ))
        .ok_or_else(|| eyre::eyre!("no MultisigProver.{chain_axelar_id} config"))?;

    let salt_key = ctx.state.cosm_salt.clone();
    let salt_bytes = get_salt_from_key(&salt_key);
    let salt_b64 = base64::engine::general_purpose::STANDARD.encode(salt_bytes.as_slice());

    let domain_separator = compute_domain_separator(&ctx.target_json, &ctx.axelar_id)?;
    let domain_sep_hex = alloy::hex::encode(domain_separator.as_slice());

    let contract_admin = match env {
        "devnet-amplifier" => "axelar1zlr7e5qf3sz7yf890rkh9tcnu87234k6k7ytd9",
        "testnet" => "axelar1wxej3l9aczsns3harrtdzk7rct29jl47tvu8mp",
        "mainnet" => "axelar1nctnr9x0qexemeld5w7w752rmqdsqqv92dw9am",
        _ => "axelar12qvsvse32cjyw60ztysd3v655aj5urqeup82ky",
    };

    let deployment_name =
        format!("{chain_axelar_id}-{gateway_code_id}-{verifier_code_id}-{prover_code_id}");

    let execute_msg = json!({
        "instantiate_chain_contracts": {
            "deployment_name": deployment_name,
            "salt": salt_b64,
            "params": {
                "manual": {
                    "gateway": {
                        "code_id": gateway_code_id,
                        "label": format!("Gateway-{chain_axelar_id}"),
                        "msg": null,
                        "contract_admin": contract_admin
                    },
                    "verifier": {
                        "code_id": verifier_code_id,
                        "label": format!("VotingVerifier-{chain_axelar_id}"),
                        "msg": {
                            "governance_address": vv_config["governanceAddress"],
                            "service_name": vv_config["serviceName"],
                            "source_gateway_address": vv_config["sourceGatewayAddress"],
                            "voting_threshold": vv_config["votingThreshold"],
                            "block_expiry": vv_config["blockExpiry"].as_u64().unwrap_or(DEFAULT_VV_BLOCK_EXPIRY).to_string(),
                            "confirmation_height": vv_config["confirmationHeight"],
                            "source_chain": chain_axelar_id,
                            "rewards_address": rewards_addr,
                            "msg_id_format": vv_config["msgIdFormat"],
                            "chain_codec_address": chain_codec_addr,
                            "address_format": vv_config["addressFormat"]
                        },
                        "contract_admin": contract_admin
                    },
                    "prover": {
                        "code_id": prover_code_id,
                        "label": format!("MultisigProver-{chain_axelar_id}"),
                        "msg": {
                            "governance_address": mp_config["governanceAddress"],
                            "admin_address": match env {
                                "testnet" => "axelar1w7y7v26rtnrj4vrx6q3qq4hfsmc68hhsxnadlf",
                                _ => mp_config.get("adminAddress").and_then(|v| v.as_str())
                                    .ok_or_else(|| eyre::eyre!("no adminAddress in MultisigProver config for {env}"))?,
                            },
                            "multisig_address": multisig_addr,
                            "signing_threshold": mp_config["signingThreshold"],
                            "service_name": mp_config["serviceName"],
                            "chain_name": chain_axelar_id,
                            "verifier_set_diff_threshold": mp_config["verifierSetDiffThreshold"],
                            "key_type": mp_config["keyType"],
                            "domain_separator": domain_sep_hex,
                            "notify_signing_session": false,
                            "expect_full_message_payloads": false,
                            "sig_verifier_address": null,
                            "chain_codec_address": chain_codec_addr
                        },
                        "contract_admin": contract_admin
                    }
                }
            }
        }
    });

    let json_str = serde_json::to_string_pretty(&execute_msg)?;
    ui::info(&format!(
        "execute msg: {}",
        ui::truncated_json(&json_str, 3)
    ));

    let sender = if use_governance {
        &governance_address
    } else {
        axelar_address
    };
    let inner_msg = build_execute_msg_any(sender, &coordinator_addr, &execute_msg)?;

    let messages = if use_governance {
        let deposit_amount = read_axelar_contract_field(
            &ctx.target_json,
            "/axelar/govProposalExpeditedDepositAmount",
        )
        .unwrap_or_else(|_| DEFAULT_PROPOSAL_DEPOSIT_UAXL.to_string());
        let title = format!("Instantiate chain contracts for {chain_axelar_id}");
        let summary = format!(
            "Instantiate Gateway, VotingVerifier and MultisigProver contracts for {chain_axelar_id} via Coordinator"
        );
        vec![build_submit_proposal_any(
            axelar_address,
            vec![inner_msg],
            &title,
            &summary,
            &deposit_amount,
            fee_denom,
            true,
        )?]
    } else {
        vec![inner_msg]
    };

    let tx_resp = sign_and_broadcast_cosmos_tx(
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        messages,
    )
    .await?;

    // Save deployment name to target json
    let content = fs::read_to_string(&ctx.target_json)?;
    let mut root: Value = serde_json::from_str(&content)?;
    let coord = root
        .pointer_mut("/axelar/contracts/Coordinator")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| eyre::eyre!("no Coordinator config"))?;
    if coord.get("deployments").is_none() {
        coord.insert("deployments".to_string(), json!({}));
    }
    coord["deployments"].as_object_mut().unwrap().insert(
        chain_axelar_id.to_string(),
        json!({
            "deploymentName": deployment_name,
            "salt": salt_key
        }),
    );

    if let Some(vv) = root.pointer_mut(&format!(
        "/axelar/contracts/VotingVerifier/{chain_axelar_id}"
    )) {
        vv["codeId"] = json!(verifier_code_id);
        vv["contractAdmin"] = json!(contract_admin);
    }
    if let Some(mp) = root.pointer_mut(&format!(
        "/axelar/contracts/MultisigProver/{chain_axelar_id}"
    )) {
        mp["codeId"] = json!(prover_code_id);
        mp["domainSeparator"] = json!(format!("0x{domain_sep_hex}"));
        mp["contractAdmin"] = json!(contract_admin);
    }
    if let Some(gw) = root.pointer_mut(&format!("/axelar/contracts/Gateway/{chain_axelar_id}")) {
        gw["codeId"] = json!(gateway_code_id);
        gw["contractAdmin"] = json!(contract_admin);
    } else if let Some(gateway_obj) = root
        .pointer_mut("/axelar/contracts/Gateway")
        .and_then(|v| v.as_object_mut())
    {
        gateway_obj.insert(
            chain_axelar_id.to_string(),
            json!({
                "codeId": gateway_code_id,
                "contractAdmin": contract_admin
            }),
        );
    }
    fs::write(
        &ctx.target_json,
        serde_json::to_string_pretty(&root)? + "\n",
    )?;

    if use_governance {
        let proposal_id = extract_proposal_id(&tx_resp)?;
        ui::kv("proposal submitted", &proposal_id.to_string());
        ui::action_required(&[
            "Vote on the proposal:",
            &format!("./vote_{env}_proposal.sh {env}-nodes {proposal_id}"),
        ]);
        ctx.state
            .proposals
            .insert(proposal_key.to_string(), proposal_id);
    } else {
        ui::success("direct execution completed");
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_register_deployment(
    ctx: &mut DeployContext,
    signing_key: &cosmrs::crypto::secp256k1::SigningKey,
    axelar_address: &str,
    lcd: &str,
    chain_id: &str,
    fee_denom: &str,
    gas_price: f64,
    use_governance: bool,
    chain_axelar_id: &str,
    env: &str,
    proposal_key: &str,
) -> Result<()> {
    ui::info(&format!("registering deployment for {chain_axelar_id}..."));

    let coordinator_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Coordinator/address")?;
    let governance_address =
        read_axelar_contract_field(&ctx.target_json, "/axelar/governanceAddress")?;

    let deployment_name = read_axelar_contract_field(
        &ctx.target_json,
        &format!("/axelar/contracts/Coordinator/deployments/{chain_axelar_id}/deploymentName"),
    )?;

    let execute_msg = json!({
        "register_deployment": {
            "deployment_name": deployment_name
        }
    });

    let sender = if use_governance {
        &governance_address
    } else {
        axelar_address
    };
    let inner_msg = build_execute_msg_any(sender, &coordinator_addr, &execute_msg)?;

    let messages = if use_governance {
        let deposit_amount = read_axelar_contract_field(
            &ctx.target_json,
            "/axelar/govProposalExpeditedDepositAmount",
        )
        .unwrap_or_else(|_| DEFAULT_PROPOSAL_DEPOSIT_UAXL.to_string());
        let title = format!("Register {chain_axelar_id} deployment on Coordinator");
        vec![build_submit_proposal_any(
            axelar_address,
            vec![inner_msg],
            &title,
            &title,
            &deposit_amount,
            fee_denom,
            true,
        )?]
    } else {
        vec![inner_msg]
    };

    let tx_resp = sign_and_broadcast_cosmos_tx(
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        messages,
    )
    .await?;

    if use_governance {
        let proposal_id = extract_proposal_id(&tx_resp)?;
        ui::kv("proposal submitted", &proposal_id.to_string());
        ui::action_required(&[
            "Vote on the proposal:",
            &format!("./vote_{env}_proposal.sh {env}-nodes {proposal_id}"),
        ]);
        ctx.state
            .proposals
            .insert(proposal_key.to_string(), proposal_id);
    } else {
        ui::success("direct execution completed");
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_create_reward_pools(
    ctx: &mut DeployContext,
    signing_key: &cosmrs::crypto::secp256k1::SigningKey,
    axelar_address: &str,
    lcd: &str,
    chain_id: &str,
    fee_denom: &str,
    gas_price: f64,
    use_governance: bool,
    chain_axelar_id: &str,
    env: &str,
    proposal_key: &str,
) -> Result<()> {
    ui::info(&format!("creating reward pools for {chain_axelar_id}..."));

    let rewards_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Rewards/address")?;
    let governance_address =
        read_axelar_contract_field(&ctx.target_json, "/axelar/governanceAddress")?;
    let multisig_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Multisig/address")?;
    let voting_verifier_addr = read_axelar_contract_field(
        &ctx.target_json,
        &format!("/axelar/contracts/VotingVerifier/{chain_axelar_id}/address"),
    )?;

    // Rewards-pool params per network. epoch_duration is in cosmos blocks
    // (testnet/stagenet share the 600 default; mainnet's 14845 ≈ 24h at
    // ~5.8s/block). participation_threshold is a ratio, rewards_per_epoch in
    // uaxl. These match `axelar-contract-deployments` epoch params for the
    // amplifier rewards pool.
    let (epoch_duration, participation_threshold, rewards_per_epoch) = match env {
        "devnet-amplifier" => ("100", json!(["7", "10"]), "100"),
        "mainnet" => ("14845", json!(["8", "10"]), "3424660000"),
        _ => ("600", json!(["7", "10"]), "100"),
    };

    let msg1 = json!({
        "create_pool": {
            "params": {
                "epoch_duration": epoch_duration,
                "participation_threshold": participation_threshold,
                "rewards_per_epoch": rewards_per_epoch
            },
            "pool_id": {
                "chain_name": chain_axelar_id,
                "contract": voting_verifier_addr
            }
        }
    });
    let msg2 = json!({
        "create_pool": {
            "params": {
                "epoch_duration": epoch_duration,
                "participation_threshold": participation_threshold,
                "rewards_per_epoch": rewards_per_epoch
            },
            "pool_id": {
                "chain_name": chain_axelar_id,
                "contract": multisig_addr
            }
        }
    });

    let sender = if use_governance {
        &governance_address
    } else {
        axelar_address
    };
    let inner_msg1 = build_execute_msg_any(sender, &rewards_addr, &msg1)?;
    let inner_msg2 = build_execute_msg_any(sender, &rewards_addr, &msg2)?;

    let messages = if use_governance {
        let deposit_amount = read_axelar_contract_field(
            &ctx.target_json,
            "/axelar/govProposalExpeditedDepositAmount",
        )
        .unwrap_or_else(|_| DEFAULT_PROPOSAL_DEPOSIT_UAXL.to_string());
        let title = format!("Create reward pools for {chain_axelar_id}");
        let summary =
            format!("Create reward pools for {chain_axelar_id} voting verifier and multisig");
        vec![build_submit_proposal_any(
            axelar_address,
            vec![inner_msg1, inner_msg2],
            &title,
            &summary,
            &deposit_amount,
            fee_denom,
            true,
        )?]
    } else {
        vec![inner_msg1, inner_msg2]
    };

    let tx_resp = sign_and_broadcast_cosmos_tx(
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        messages,
    )
    .await?;

    if use_governance {
        let proposal_id = extract_proposal_id(&tx_resp)?;
        ui::kv("proposal submitted", &proposal_id.to_string());
        ui::action_required(&[
            "Vote on the proposal:",
            &format!("./vote_{env}_proposal.sh {env}-nodes {proposal_id}"),
        ]);
        ctx.state
            .proposals
            .insert(proposal_key.to_string(), proposal_id);
    } else {
        ui::success("direct execution completed");
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_add_rewards(
    ctx: &DeployContext,
    signing_key: &cosmrs::crypto::secp256k1::SigningKey,
    axelar_address: &str,
    lcd: &str,
    chain_id: &str,
    fee_denom: &str,
    gas_price: f64,
    chain_axelar_id: &str,
) -> Result<()> {
    ui::info(&format!("adding rewards for {chain_axelar_id}..."));

    let rewards_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Rewards/address")?;
    let multisig_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Multisig/address")?;
    let voting_verifier_addr = read_axelar_contract_field(
        &ctx.target_json,
        &format!("/axelar/contracts/VotingVerifier/{chain_axelar_id}/address"),
    )?;

    let reward_amount = DEFAULT_REWARD_AMOUNT_UAXL;
    let funds = vec![ProtoCoin {
        denom: fee_denom.to_string(),
        amount: reward_amount.to_string(),
    }];

    let msg1 = json!({
        "add_rewards": {
            "pool_id": {
                "chain_name": chain_axelar_id,
                "contract": multisig_addr
            }
        }
    });
    let msg2 = json!({
        "add_rewards": {
            "pool_id": {
                "chain_name": chain_axelar_id,
                "contract": voting_verifier_addr
            }
        }
    });

    let inner_msg1 =
        build_execute_msg_any_with_funds(axelar_address, &rewards_addr, &msg1, funds.clone())?;
    let inner_msg2 = build_execute_msg_any_with_funds(axelar_address, &rewards_addr, &msg2, funds)?;

    ui::info(&format!(
        "sending {reward_amount}{fee_denom} to each reward pool"
    ));
    let tx_resp = sign_and_broadcast_cosmos_tx(
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        vec![inner_msg1, inner_msg2],
    )
    .await?;

    let code = tx_resp
        .pointer("/tx_response/code")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if code != 0 {
        let raw_log = tx_resp
            .pointer("/tx_response/raw_log")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        return Err(eyre::eyre!(
            "add_rewards tx failed (code {code}): {raw_log}"
        ));
    }
    ui::success("rewards added to both pools");

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_register_its_on_hub(
    ctx: &mut DeployContext,
    signing_key: &cosmrs::crypto::secp256k1::SigningKey,
    axelar_address: &str,
    lcd: &str,
    chain_id: &str,
    fee_denom: &str,
    gas_price: f64,
    use_governance: bool,
    chain_axelar_id: &str,
    env: &str,
    proposal_key: &str,
) -> Result<()> {
    ui::info(&format!("registering {chain_axelar_id} on ITS Hub..."));

    let its_hub_addr = read_axelar_contract_field(
        &ctx.target_json,
        "/axelar/contracts/InterchainTokenService/address",
    )?;
    let governance_address =
        read_axelar_contract_field(&ctx.target_json, "/axelar/governanceAddress")?;

    // Read the ITS edge contract (EVM proxy address) from target JSON
    let content = fs::read_to_string(&ctx.target_json)?;
    let root: Value = serde_json::from_str(&content)?;
    let its_edge_contract = root
        .pointer(&format!(
            "/chains/{}/contracts/InterchainTokenService/address",
            ctx.axelar_id
        ))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            eyre::eyre!(
                "no InterchainTokenService address for {} — run DeployInterchainTokenService first",
                ctx.axelar_id
            )
        })?
        .to_string();

    // Read msg_translator (ItsAbiTranslator address)
    let msg_translator = root
        .pointer("/axelar/contracts/ItsAbiTranslator/address")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("no axelar.contracts.ItsAbiTranslator.address in target JSON"))?
        .to_string();

    let execute_msg = json!({
        "register_chains": {
            "chains": [{
                "chain": chain_axelar_id,
                "its_edge_contract": its_edge_contract,
                "msg_translator": msg_translator,
                "truncation": {
                    "max_uint_bits": 256,
                    "max_decimals_when_truncating": 255
                }
            }]
        }
    });

    let json_str = serde_json::to_string_pretty(&execute_msg)?;
    ui::info(&format!(
        "execute msg: {}",
        ui::truncated_json(&json_str, 3)
    ));

    let sender = if use_governance {
        &governance_address
    } else {
        axelar_address
    };
    let inner_msg = build_execute_msg_any(sender, &its_hub_addr, &execute_msg)?;

    let messages = if use_governance {
        let deposit_amount = read_axelar_contract_field(
            &ctx.target_json,
            "/axelar/govProposalExpeditedDepositAmount",
        )
        .unwrap_or_else(|_| DEFAULT_PROPOSAL_DEPOSIT_UAXL.to_string());
        let title = format!("Register {chain_axelar_id} on ITS Hub");
        let summary = format!(
            "Register {chain_axelar_id} ITS edge contract ({its_edge_contract}) on InterchainTokenService Hub"
        );
        vec![build_submit_proposal_any(
            axelar_address,
            vec![inner_msg],
            &title,
            &summary,
            &deposit_amount,
            fee_denom,
            true,
        )?]
    } else {
        vec![inner_msg]
    };

    let tx_resp = sign_and_broadcast_cosmos_tx(
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        messages,
    )
    .await?;

    if use_governance {
        let proposal_id = extract_proposal_id(&tx_resp)?;
        ui::kv("proposal submitted", &proposal_id.to_string());
        ui::action_required(&[
            "Vote on the proposal:",
            &format!("./vote_{env}_proposal.sh {env}-nodes {proposal_id}"),
        ]);
        ctx.state
            .proposals
            .insert(proposal_key.to_string(), proposal_id);
    } else {
        ui::success("direct execution completed");
    }

    Ok(())
}
