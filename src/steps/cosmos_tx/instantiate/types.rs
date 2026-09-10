use serde::Deserialize;
use serde_json::Value;

pub(super) struct ChainContractAddresses {
    pub coordinator: String,
    pub rewards: String,
    pub multisig: String,
    pub codec: String,
    pub governance: String,
}

pub(super) struct ChainCodeIds {
    pub gateway: u64,
    pub verifier: u64,
    pub prover: u64,
}

pub(super) struct InstantiatePlan {
    pub execute_msg: Value,
    pub deployment_name: String,
    pub salt_key: String,
    pub domain_separator: String,
    pub contract_admin: &'static str,
    pub codes: ChainCodeIds,
}

#[derive(Debug, Deserialize)]
pub(super) struct Deployment {
    pub chain_name: String,
    pub gateway_address: String,
    pub verifier_address: String,
    pub prover_address: String,
}

#[derive(Deserialize)]
pub(super) struct ContractResponse {
    pub contract_info: ContractInfo,
}

#[derive(Deserialize)]
pub(super) struct ContractInfo {
    pub code_id: String,
    pub creator: String,
    pub admin: String,
    pub label: String,
}

#[derive(Deserialize)]
pub(super) struct RawResponse {
    pub data: String,
}

#[derive(Deserialize)]
pub(super) struct VerifierConfig {
    pub source_chain: String,
    pub source_gateway_address: String,
}

#[derive(Deserialize)]
pub(super) struct ProverConfig {
    pub chain_name: String,
    pub gateway: String,
    pub voting_verifier: String,
    pub domain_separator: [u8; 32],
}

#[derive(Deserialize)]
pub(super) struct QueryErrorBody {
    pub code: u32,
    pub message: String,
}

#[derive(Deserialize)]
pub(super) struct Proposal {
    pub status: String,
    #[serde(default)]
    pub failed_reason: String,
}
