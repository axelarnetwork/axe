//! Recover matching proposals from the hub instead of spending another deposit.

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::SolValue;
use base64::Engine;
use eyre::{Result, WrapErr};
use serde_json::Value;

use crate::{cosmos::lcd_query_proposal, http};

use super::recovery_types::{ExecuteContract, ExistingProposal, GatewayCall, ProposalPage};
use super::types::{ProposalPlan, ProposalType, ProposeArgs, ResolvedConfig};

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;

impl ExistingProposal {
    pub fn number(&self) -> Result<u64> {
        self.id.parse().wrap_err("invalid proposal ID from LCD")
    }

    pub fn execution_time(&self) -> Result<chrono::DateTime<chrono::FixedOffset>> {
        chrono::DateTime::parse_from_rfc3339(
            self.voting_end_time
                .as_deref()
                .ok_or_else(|| eyre::eyre!("proposal has no voting_end_time"))?,
        )
        .wrap_err("invalid proposal voting_end_time")
    }

    /// Only accept a single, zero-funds governance call to this network's gateway.
    #[cfg(test)]
    pub fn matching_payload(
        &self,
        cfg: &ResolvedConfig,
        ptype: ProposalType,
        target: Address,
        calldata: &Bytes,
    ) -> Option<Bytes> {
        if self.messages.len() != 1 {
            return None;
        }
        message_payload(&self.messages[0], cfg, ptype, target, calldata)
    }

    /// Every call in the plan matched by exactly one message, with no message
    /// left over. Payloads come back in plan order, whatever the message order.
    pub fn matching_plan(&self, plan: &ProposalPlan) -> Option<Vec<Bytes>> {
        if self.messages.len() != plan.calls.len() {
            return None;
        }
        let mut used = vec![false; self.messages.len()];
        let mut payloads = Vec::with_capacity(plan.calls.len());
        for call in &plan.calls {
            let (index, payload) = self.messages.iter().enumerate().find_map(|(i, message)| {
                if used[i] {
                    return None;
                }
                message_payload(message, &call.cfg, plan.ptype, call.target, &call.calldata)
                    .map(|payload| (i, payload))
            })?;
            used[index] = true;
            payloads.push(payload);
        }
        Some(payloads)
    }
}

/// The governance payload of one zero-funds `call_contract` message, if it is
/// this edge's ASG command for exactly `(target, calldata)`.
fn message_payload(
    message: &Value,
    cfg: &ResolvedConfig,
    ptype: ProposalType,
    target: Address,
    calldata: &Bytes,
) -> Option<Bytes> {
    let message: ExecuteContract = serde_json::from_value(message.clone()).ok()?;
    if message.message_type != "/cosmwasm.wasm.v1.MsgExecuteContract"
        || message.sender != cfg.gov_module
        || message.contract != cfg.axelarnet_gateway
        || !message.funds.is_empty()
    {
        return None;
    }
    let call: GatewayCall = match message.msg {
        Value::String(encoded) => serde_json::from_slice(
            &base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()?,
        )
        .ok()?,
        object => serde_json::from_value(object).ok()?,
    };
    if call.call_contract.destination_chain != cfg.edge_axelar_id
        || !call
            .call_contract
            .destination_address
            .eq_ignore_ascii_case(&cfg.asg_address)
    {
        return None;
    }
    let payload: Bytes = alloy::hex::decode(call.call_contract.payload).ok()?.into();
    let (command, actual_target, actual_calldata, value, _) =
        <(U256, Address, Bytes, U256, U256)>::abi_decode_params_validate(&payload).ok()?;
    (command == U256::from(ptype.command())
        && actual_target == target
        && actual_calldata == *calldata
        && value == U256::ZERO)
        .then_some(payload)
}

pub async fn load(lcd: &str, id: u64) -> Result<ExistingProposal> {
    serde_json::from_value(lcd_query_proposal(lcd, id).await?)
        .wrap_err("invalid governance proposal response")
}

/// Failing to read history must fail closed. Never treat an outage as no match.
pub async fn find(
    plan: &ProposalPlan,
    args: &ProposeArgs,
) -> Result<Option<(ExistingProposal, Vec<Bytes>)>> {
    let lcd = &plan.hub().lcd;
    if let Some(id) = args.proposal_id {
        let proposal = load(lcd, id).await?;
        let payloads = proposal
            .matching_plan(plan)
            .ok_or_else(|| eyre::eyre!("proposal {id} does not match this governance call"))?;
        return Ok(Some((proposal, payloads)));
    }
    if args.new_proposal {
        return Ok(None);
    }
    let mut next_key = String::new();
    loop {
        let mut url: reqwest::Url =
            format!("{}/cosmos/gov/v1/proposals", lcd.trim_end_matches('/')).parse()?;
        url.query_pairs_mut()
            // Historical code-upload proposals can exceed the LCD's 10 MiB
            // gRPC response limit when fetched in batches of 100.
            .append_pair("pagination.limit", "20")
            .append_pair("pagination.reverse", "true")
            .append_pair("pagination.key", &next_key);
        let page: ProposalPage = http::get_json(url)
            .await
            .wrap_err("cannot check existing proposals; refusing to submit a possible duplicate")?;
        for proposal in page.proposals {
            if let Some(payloads) = proposal.matching_plan(plan) {
                // Even a failed/rejected match needs an explicit --new-proposal.
                return Ok(Some((proposal, payloads)));
            }
        }
        let Some(key) = page.pagination.next_key.filter(|key| !key.is_empty()) else {
            return Ok(None);
        };
        eyre::ensure!(
            key != next_key,
            "LCD repeated its pagination cursor; refusing to submit"
        );
        next_key = key;
    }
}
