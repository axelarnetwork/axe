//! Use the signer's deposit, falling back to an existing CPI proposal grant on mainnet.

use chrono::{DateTime, Utc};
use cosmos_sdk_proto::cosmos::{authz::v1beta1::MsgExec, gov::v1::MsgSubmitProposal};
use cosmrs::{AccountId, Any};
use eyre::{Result, WrapErr};
use prost::Message;

use crate::cosmos::rpc::lcd_query_balance;
use crate::types::Network;
use crate::{http, ui};

use super::funding_types::{Authorization, GrantsResponse, ProposalFunding};
use super::types::ResolvedConfig;
use super::{GAS_BUFFER_UAXL, format_deposit};

const CPI_MULTISIG: &str = "axelar14vps3ev03zyp2wmj89etx8rrxdxyltfy4rzl5m";
const SUBMIT_PROPOSAL: &str = "/cosmos.gov.v1.MsgSubmitProposal";

pub(super) async fn resolve(
    network: Network,
    hub: &ResolvedConfig,
    submitter: &str,
    deposit: u128,
) -> Result<ProposalFunding> {
    let signer: AccountId = submitter.parse()?;
    let balance = lcd_query_balance(&hub.lcd, submitter, &hub.fee_denom).await?;
    if balance >= deposit + GAS_BUFFER_UAXL {
        return Ok(ProposalFunding::Direct { signer });
    }
    eyre::ensure!(
        network == Network::Mainnet && hub.chain_id == "axelar-dojo-1",
        "proposal signer {signer} has {}; need {} for the deposit and gas",
        format_deposit(&balance.to_string(), &hub.fee_denom),
        format_deposit(&(deposit + GAS_BUFFER_UAXL).to_string(), &hub.fee_denom)
    );
    eyre::ensure!(
        balance >= GAS_BUFFER_UAXL,
        "proposal signer {signer} needs at least {} for transaction fees; CPI only pays the deposit",
        format_deposit(&GAS_BUFFER_UAXL.to_string(), &hub.fee_denom)
    );
    let granter: AccountId = CPI_MULTISIG.parse()?;
    check_grant(&hub.lcd, &granter, &signer).await?;
    let available = lcd_query_balance(&hub.lcd, granter.as_ref(), &hub.fee_denom).await?;
    eyre::ensure!(
        available >= deposit,
        "CPI multisig {granter} has {}; need {} for the proposal deposit",
        format_deposit(&available.to_string(), &hub.fee_denom),
        format_deposit(&deposit.to_string(), &hub.fee_denom)
    );
    Ok(ProposalFunding::Authorized { signer, granter })
}

async fn check_grant(lcd: &str, granter: &AccountId, signer: &AccountId) -> Result<()> {
    let mut url: reqwest::Url =
        format!("{}/cosmos/authz/v1beta1/grants", lcd.trim_end_matches('/')).parse()?;
    url.query_pairs_mut()
        .append_pair("granter", granter.as_ref())
        .append_pair("grantee", signer.as_ref())
        .append_pair("msg_type_url", SUBMIT_PROPOSAL);
    let response: GrantsResponse = http::get_json(url)
        .await
        .wrap_err("cannot verify CPI proposal authorization; refusing deposit fallback")?;
    eyre::ensure!(
        has_active_grant(&response, Utc::now())?,
        "signer {signer} cannot cover the deposit and has no active CPI multisig authorization for {SUBMIT_PROPOSAL}"
    );
    Ok(())
}

fn has_active_grant(response: &GrantsResponse, now: DateTime<Utc>) -> Result<bool> {
    for grant in &response.grants {
        if !matches!(&grant.authorization, Authorization::Generic { msg } if msg == SUBMIT_PROPOSAL)
        {
            continue;
        }
        let active = match &grant.expiration {
            None => true,
            Some(expiration) => {
                DateTime::parse_from_rfc3339(expiration)
                    .wrap_err("invalid CPI authorization expiration")?
                    > now
            }
        };
        if active {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn review(funding: &ProposalFunding) {
    if let ProposalFunding::Authorized { signer, granter } = funding {
        ui::warn(&format!(
            "Signing wallet cannot cover the proposal deposit and fee buffer. Using CPI multisig {granter} via its existing proposal authorization. {signer} pays transaction fees; any deposit refund returns to CPI multisig."
        ));
    }
    ui::address("proposer / deposit payer", funding.proposer().as_ref());
    ui::address("signer / fee payer", funding.signer().as_ref());
}

/// Only the proposal is delegated. Its governance messages keep the gov module as sender.
pub(super) fn submission(funding: &ProposalFunding, proposal: Any) -> Result<Any> {
    eyre::ensure!(
        proposal.type_url == SUBMIT_PROPOSAL,
        "expected a governance proposal"
    );
    let decoded = MsgSubmitProposal::decode(proposal.value.as_slice())?;
    eyre::ensure!(
        decoded.proposer == funding.proposer().as_ref(),
        "proposal deposit payer does not match the reviewed funding"
    );
    match funding {
        ProposalFunding::Direct { .. } => Ok(proposal),
        ProposalFunding::Authorized { signer, .. } => Ok(Any {
            type_url: "/cosmos.authz.v1beta1.MsgExec".into(),
            value: MsgExec {
                grantee: signer.to_string(),
                msgs: vec![tendermint_proto::google::protobuf::Any {
                    type_url: proposal.type_url,
                    value: proposal.value,
                }],
            }
            .encode_to_vec(),
        }),
    }
}

#[cfg(test)]
#[path = "funding_tests.rs"]
mod tests;
