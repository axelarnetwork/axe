//! Recover matching proposals from the hub instead of spending another deposit.

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::SolValue;
use base64::Engine;
use eyre::{Result, WrapErr};
use serde_json::Value;

use crate::{cosmos::lcd_query_proposal, http};

use super::recovery_types::{ExecuteContract, ExistingProposal, GatewayCall, ProposalPage};
use super::types::{ProposalType, ProposeArgs, ResolvedConfig};

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
        let message: ExecuteContract = serde_json::from_value(self.messages[0].clone()).ok()?;
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
}

pub async fn load(lcd: &str, id: u64) -> Result<ExistingProposal> {
    serde_json::from_value(lcd_query_proposal(lcd, id).await?)
        .wrap_err("invalid governance proposal response")
}

/// Failing to read history must fail closed. Never treat an outage as no match.
pub async fn find(
    cfg: &ResolvedConfig,
    args: &ProposeArgs,
    target: Address,
    calldata: &Bytes,
) -> Result<Option<(ExistingProposal, Bytes)>> {
    if let Some(id) = args.proposal_id {
        let proposal = load(&cfg.lcd, id).await?;
        let payload = proposal
            .matching_payload(cfg, args.proposal_type, target, calldata)
            .ok_or_else(|| eyre::eyre!("proposal {id} does not match this governance call"))?;
        return Ok(Some((proposal, payload)));
    }
    if args.new_proposal {
        return Ok(None);
    }
    let mut next_key = String::new();
    loop {
        let mut url: reqwest::Url =
            format!("{}/cosmos/gov/v1/proposals", cfg.lcd.trim_end_matches('/')).parse()?;
        url.query_pairs_mut()
            .append_pair("pagination.limit", "100")
            .append_pair("pagination.reverse", "true")
            .append_pair("pagination.key", &next_key);
        let page: ProposalPage = http::get_json(url)
            .await
            .wrap_err("cannot check existing proposals; refusing to submit a possible duplicate")?;
        for proposal in page.proposals {
            if let Some(payload) =
                proposal.matching_payload(cfg, args.proposal_type, target, calldata)
            {
                // Even a failed/rejected match needs an explicit --new-proposal.
                return Ok(Some((proposal, payload)));
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
