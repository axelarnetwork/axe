//! LCD response types for proposal recovery.
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct ExistingProposal {
    pub id: String,
    pub status: String,
    pub voting_end_time: Option<String>,
    pub messages: Vec<Value>,
}

#[derive(Deserialize)]
pub(super) struct ProposalPage {
    pub proposals: Vec<ExistingProposal>,
    pub pagination: Pagination,
}

#[derive(Deserialize)]
pub(super) struct Pagination {
    pub next_key: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ExecuteContract {
    #[serde(rename = "@type")]
    pub message_type: String,
    pub sender: String,
    pub contract: String,
    pub msg: Value,
    pub funds: Vec<Value>,
}

#[derive(Deserialize)]
pub(super) struct GatewayCall {
    pub call_contract: CallContract,
}

#[derive(Deserialize)]
pub(super) struct CallContract {
    pub destination_chain: String,
    pub destination_address: String,
    pub payload: String,
}
