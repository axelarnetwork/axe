use base64::Engine;
use eyre::Result;
use prost::Message;
use serde_json::{Value, json};

use super::session::pause;
use super::types::{GovParams, ParamsResponse};
use crate::commands::deploy::DeployContext;
use crate::cosmos::{lcd_query_proposal, read_axelar_config};
use crate::ui;

pub async fn parameters(lcd: &str) -> Result<GovParams> {
    let response: ParamsResponse = crate::http::client()
        .get(format!(
            "{}/cosmos/gov/v1/params/voting",
            lcd.trim_end_matches('/')
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(response.params)
}

pub async fn deposit(lcd: &str, denom: &str) -> Result<String> {
    let params = parameters(lcd).await?;
    eyre::ensure!(
        params.expedited_min_deposit.len() == 1,
        "unsupported multi-denom governance deposit"
    );
    let coin = params
        .expedited_min_deposit
        .into_iter()
        .next()
        .ok_or_else(|| eyre::eyre!("missing deposit"))?;
    eyre::ensure!(
        coin.denom == denom,
        "governance deposit denomination mismatch"
    );
    eyre::ensure!(
        coin.amount.parse::<u128>()? > 0,
        "invalid governance deposit"
    );

    Ok(coin.amount)
}

pub async fn check(ctx: &DeployContext, key: &str) -> Result<()> {
    if !ctx.state.env.deployment_uses_governance() {
        return super::direct::check(ctx, key).await;
    }
    let id = ctx
        .state
        .proposals
        .get(key)
        .ok_or_else(|| eyre::eyre!("missing proposal for {key}"))?;
    let (lcd, _, _, _) = read_axelar_config(&ctx.target_json).await?;
    let proposal = lcd_query_proposal(&lcd, *id).await?;
    validate_identity(ctx, key, *id, &proposal, &lcd).await?;
    print_proposal(*id, &proposal);
    match proposal["status"].as_str() {
        Some("PROPOSAL_STATUS_PASSED") => Ok(()),
        Some("PROPOSAL_STATUS_VOTING_PERIOD") => {
            super::handoff::proposal(&ctx.state, *id, proposal["voting_end_time"].as_str());
            Err(pause(format!(
                "Deployment paused until proposal {id} passes"
            )))
        }
        Some("PROPOSAL_STATUS_DEPOSIT_PERIOD") => Err(pause(format!(
            "Proposal {id} is still in its deposit period; inspect its deposit requirements before voting. No replacement was submitted"
        ))),
        _ => {
            eyre::bail!(
                "Proposal {id} cannot advance this deployment (status: {}, reason: {}). Review its governance outcome with the network operators. Rerunning axe will not replace this proposal; keep the state and journal for recovery",
                proposal["status"],
                proposal["failed_reason"]
            );
        }
    }
}

fn print_proposal(id: u64, proposal: &Value) {
    ui::kv("proposal", &id.to_string());
    for field in [
        "status",
        "expedited",
        "voting_end_time",
        "total_deposit",
        "final_tally_result",
        "failed_reason",
    ] {
        ui::kv(field, &proposal[field].to_string());
    }
}

pub async fn status(state: &crate::state::State, votes: bool) -> Result<()> {
    let (lcd, _, _, _) = read_axelar_config(&state.target_json).await?;
    for id in state.proposals.values() {
        print_proposal(*id, &lcd_query_proposal(&lcd, *id).await?);
        let tally: Value = crate::http::client()
            .get(format!(
                "{}/cosmos/gov/v1/proposals/{id}/tally",
                lcd.trim_end_matches('/')
            ))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ui::kv("current tally", &tally["tally"].to_string());
        if !votes {
            ui::info("Use deploy status --votes to include individual voters.");
            continue;
        }
        let mut next = String::new();
        loop {
            let mut request = crate::http::client().get(format!(
                "{}/cosmos/gov/v1/proposals/{id}/votes",
                lcd.trim_end_matches('/')
            ));
            if !next.is_empty() {
                request = request.query(&[("pagination.key", &next)]);
            }
            let page: Value = request.send().await?.error_for_status()?.json().await?;
            ui::info(&serde_json::to_string_pretty(&page["votes"])?);
            let key = page
                .pointer("/pagination/next_key")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if key.is_empty() {
                break;
            }
            eyre::ensure!(key != next, "vote pagination did not advance");
            next = key.into();
        }
    }
    Ok(())
}

pub async fn recover(ctx: &mut DeployContext, name: &str, key: &str) -> Result<bool> {
    let Some(saved @ super::types::Transaction::Cosmos { .. }) = super::session::current()?
        .get(&format!("{name}/cosmos"))
        .await
    else {
        return Ok(false);
    };
    let (lcd, _, _, _) = read_axelar_config(&ctx.target_json).await?;
    let response = super::cosmos::resume(&lcd, &saved, &format!("{name}/cosmos")).await?;
    if !ctx.state.env.deployment_uses_governance() {
        super::direct::validate_submission(&saved)?;
        ui::success("Recovered journaled direct execution");
        return Ok(true);
    }
    let id = crate::cosmos::extract_proposal_id(&response)?;
    ctx.state.proposals.insert(key.into(), id);
    ui::kv("recovered proposal", &id.to_string());
    Ok(true)
}

async fn validate_identity(
    ctx: &DeployContext,
    key: &str,
    id: u64,
    proposal: &Value,
    lcd: &str,
) -> Result<()> {
    let name = match key {
        "instantiate" => "InstantiateChainContracts",
        "register" => "RegisterDeployment",
        "itsHubRegister" => "RegisterItsOnHub",
        _ => {
            eyre::bail!("unexpected proposal key");
        }
    };
    let Some(saved @ super::types::Transaction::Cosmos { .. }) = super::session::current()?
        .get(&format!("{name}/cosmos"))
        .await
    else {
        eyre::bail!("proposal has no signed transaction journal");
    };
    let response = super::cosmos::resume(lcd, &saved, &format!("{name}/cosmos")).await?;
    eyre::ensure!(
        crate::cosmos::extract_proposal_id(&response)? == id,
        "proposal ID does not match recorded submission"
    );
    if let super::types::Transaction::Cosmos { raw, .. } = &saved {
        validate_payload(raw, proposal)?;
    }
    eyre::ensure!(
        ctx.state.proposals.get(key) == Some(&id),
        "proposal identity mismatch"
    );
    Ok(())
}

pub(super) fn validate_payload(raw: &[u8], proposal: &Value) -> Result<()> {
    let tx = cosmos_sdk_proto::cosmos::tx::v1beta1::TxRaw::decode(raw)?;
    let body = cosmos_sdk_proto::cosmos::tx::v1beta1::TxBody::decode(tx.body_bytes.as_slice())?;
    eyre::ensure!(
        body.messages.len() == 1,
        "unexpected proposal transaction shape"
    );
    let message = &body.messages[0];
    eyre::ensure!(
        message.type_url == "/cosmos.gov.v1.MsgSubmitProposal",
        "unexpected proposal message"
    );
    let expected =
        cosmos_sdk_proto::cosmos::gov::v1::MsgSubmitProposal::decode(message.value.as_slice())?;
    let actual = proposal["messages"]
        .as_array()
        .ok_or_else(|| eyre::eyre!("missing proposal messages"))?;
    eyre::ensure!(
        actual.len() == expected.messages.len()
            && proposal["title"] == expected.title
            && proposal["expedited"] == expected.expedited,
        "proposal content differs from approved submission"
    );
    for (actual, message) in actual.iter().zip(expected.messages) {
        eyre::ensure!(
            message.type_url == "/cosmwasm.wasm.v1.MsgExecuteContract"
                && actual["@type"] == message.type_url,
            "unsupported governance execution"
        );
        let expected = cosmos_sdk_proto::cosmwasm::wasm::v1::MsgExecuteContract::decode(
            message.value.as_slice(),
        )?;
        let payload: Value = match actual["msg"].as_str() {
            Some(base64) => {
                serde_json::from_slice(&base64::engine::general_purpose::STANDARD.decode(base64)?)?
            }
            None => actual["msg"].clone(),
        };
        let funds: Vec<_> = expected
            .funds
            .iter()
            .map(|coin| json!({"denom":coin.denom,"amount":coin.amount}))
            .collect();
        eyre::ensure!(
            actual["sender"] == expected.sender
                && actual["contract"] == expected.contract
                && payload == serde_json::from_slice::<Value>(&expected.msg)?
                && actual["funds"] == json!(funds),
            "governance execution differs from approved submission"
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "governance_tests.rs"]
mod tests;
