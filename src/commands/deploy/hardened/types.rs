use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::{fmt, str::FromStr};

use alloy::primitives::{Address, B256, Bytes};
use alloy::rpc::types::TransactionReceipt;
use clap::Args;
use serde::{Deserialize, Serialize};

/// Public deployment inputs. Credentials are supplied through the environment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Plan {
    pub evm_chain_id: u64,
    pub axelar_chain_id: String,
    pub gateway_owner: Address,
    pub operators_owner: Address,
    pub gas_service_owner: Address,
    pub its_owner: Address,
    pub factory_owner: Address,
    pub gateway_operator: Address,
    pub prover_admin: String,
    /// Retained only to preserve fingerprints of existing journals. Not an allowlist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approved_verifiers: Vec<String>,
    /// Maximum aggregate gas liability per EVM signer, in native base units.
    pub evm_gas_budget: String,
    /// Aggregate Cosmos fee allowance per signer, in the configured fee denom.
    pub cosmos_fee_budget: String,
    pub reward_amount: String,
    pub voting_threshold: [u64; 2],
    pub signing_threshold: [u64; 2],
    pub block_expiry: u64,
    pub confirmation_height: u64,
}

/// Receipt inclusion depth (the inclusion block counts as one), or RPC finality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum EvmConfirmationPolicy {
    Confirmations(NonZeroU64),
    Finalized,
}

impl Default for EvmConfirmationPolicy {
    fn default() -> Self {
        Self::Confirmations(NonZeroU64::MIN)
    }
}

impl EvmConfirmationPolicy {
    /// Journals written before configurable confirmations required finality.
    pub const fn previous_default() -> Self {
        Self::Finalized
    }

    pub const fn block_tag(self) -> alloy::eips::BlockNumberOrTag {
        match self {
            Self::Confirmations(_) => alloy::eips::BlockNumberOrTag::Latest,
            Self::Finalized => alloy::eips::BlockNumberOrTag::Finalized,
        }
    }
}

impl FromStr for EvmConfirmationPolicy {
    type Err = eyre::Report;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "finalized" {
            return Ok(Self::Finalized);
        }
        value
            .parse::<NonZeroU64>()
            .map(Self::Confirmations)
            .map_err(|_| {
                eyre::eyre!("EVM confirmations must be a positive block count or 'finalized'")
            })
    }
}

impl TryFrom<String> for EvmConfirmationPolicy {
    type Error = eyre::Report;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl fmt::Display for EvmConfirmationPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Confirmations(count) => count.fmt(formatter),
            Self::Finalized => formatter.write_str("finalized"),
        }
    }
}

impl From<EvmConfirmationPolicy> for String {
    fn from(policy: EvmConfirmationPolicy) -> Self {
        policy.to_string()
    }
}

#[derive(Debug, Clone, Default, Args)]
pub struct Options {
    /// Optional public JSON plan instead of the hardened deployment settings in .env.
    #[arg(long)]
    pub plan: Option<PathBuf>,
    /// Continue at the verifier checkpoint, then deploy the gateway and finish deployment.
    #[arg(long)]
    pub activate: bool,
    /// EVM confirmations: positive block count or 'finalized'. New runs default to 1. Resumes inherit the journal.
    #[arg(
        long,
        env = "EVM_CONFIRMATIONS",
        hide_env_values = true,
        value_name = "COUNT|finalized"
    )]
    pub evm_confirmations: Option<EvmConfirmationPolicy>,
    /// Maximum wait for EVM inclusion/confirmation in seconds (default: 1800).
    #[arg(long, default_value_t = 1800)]
    pub evm_wait_seconds: u64,
    /// Approve a fee-only replacement for this journal action (step/transaction label).
    #[arg(long, conflicts_with = "retry_failed")]
    pub bump_fees: Option<String>,
    /// New total Cosmos fee in base units. Required when bumping a Cosmos action.
    #[arg(long, requires = "bump_fees")]
    pub cosmos_fee: Option<u128>,
    /// Retire a proven failed transaction and approve a safe new attempt.
    #[arg(long)]
    pub retry_failed: Option<String>,
    /// Gas limit for the next EVM attempt after an explicitly approved failed-transaction retry.
    #[arg(long, requires = "retry_failed", value_parser = clap::value_parser!(u64).range(1..))]
    pub retry_gas_limit: Option<u64>,
    /// Read missing Cosmos transaction evidence from this same-network archival LCD.
    #[arg(long)]
    pub recovery_lcd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Journal {
    pub version: u32,
    pub fingerprint: B256,
    #[serde(default)]
    pub inputs_hash: Option<B256>,
    #[serde(default = "EvmConfirmationPolicy::previous_default")]
    pub evm_confirmations: EvmConfirmationPolicy,
    pub actions: BTreeMap<String, Transaction>,
    #[serde(default)]
    pub evidence: BTreeMap<String, ContractEvidence>,
    #[serde(default)]
    pub confirmations: BTreeMap<String, Confirmation>,
    #[serde(default)]
    pub attempts: BTreeMap<String, Vec<Transaction>>,
    #[serde(default)]
    pub retry_gas_limits: BTreeMap<String, u64>,
    #[serde(default)]
    pub protocols: BTreeMap<String, ProtocolIdentity>,
    #[serde(default)]
    pub protocol_history: Vec<BTreeMap<String, ProtocolIdentity>>,
    #[serde(default)]
    pub initial_signers: Option<InitialSigners>,
    #[serde(default)]
    pub rotations: BTreeMap<u64, B256>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "network", rename_all = "camelCase")]
pub enum Transaction {
    Evm {
        intent: B256,
        raw: Bytes,
        hash: B256,
        sender: Address,
        nonce: u64,
        gas_cost: String,
    },
    Cosmos {
        intent: B256,
        raw: Vec<u8>,
        hash: String,
        sender: String,
        fee: String,
        sequence: u64,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Paused(pub String);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContractEvidence {
    pub address: Address,
    pub code_hash: B256,
    pub owner: Option<Address>,
    pub implementation: Option<Address>,
    pub operator: Option<Address>,
    pub signer_hash: Option<B256>,
}

#[derive(Deserialize)]
pub struct ActiveVerifier {
    pub verifier_info: VerifierIdentity,
}
#[derive(Deserialize)]
pub struct VerifierIdentity {
    pub address: String,
}

#[derive(Deserialize)]
pub struct Service {
    pub min_num_verifiers: u64,
    pub max_num_verifiers: Option<u64>,
}

#[derive(Deserialize)]
pub struct RawResponse {
    pub data: String,
}
#[derive(Deserialize)]
pub struct ContractInfoResponse {
    pub contract_info: ContractInfo,
}
#[derive(Deserialize)]
pub struct ContractInfo {
    pub code_id: String,
    pub creator: String,
    pub admin: String,
}
#[derive(Deserialize)]
pub struct GatewayConfig {
    pub router: String,
    pub verifier: String,
}
#[derive(Deserialize)]
pub struct VerifierConfig {
    pub source_chain: String,
    pub source_gateway_address: String,
    pub voting_threshold: [String; 2],
    pub block_expiry: String,
    pub confirmation_height: u64,
    pub service_name: String,
    pub msg_id_format: String,
    pub service_registry_contract: String,
    pub rewards_contract: String,
    pub chain_codec_address: String,
}
#[derive(Deserialize)]
pub struct ProverConfig {
    pub chain_name: String,
    pub signing_threshold: [String; 2],
    pub service_name: String,
    pub key_type: String,
    pub domain_separator: [u8; 32],
    pub verifier_set_diff_threshold: u64,
    pub notify_signing_session: bool,
    pub expect_full_message_payloads: bool,
    pub gateway: String,
    pub voting_verifier: String,
    pub multisig: String,
    pub coordinator: String,
    pub service_registry: String,
    pub chain_codec: String,
}
#[derive(Deserialize, Serialize)]
pub struct ProtocolContracts {
    pub router: String,
    pub multisig: String,
    pub service_registry: String,
}
#[derive(Deserialize, Serialize)]
pub struct RewardsConfig {
    pub rewards_denom: String,
}

#[derive(Deserialize)]
pub struct ProxyConfig {
    pub coordinator: String,
}

#[derive(Deserialize)]
pub struct Coin {
    pub denom: String,
    pub amount: String,
}
#[derive(Deserialize)]
pub struct GovParams {
    pub expedited_min_deposit: Vec<Coin>,
    pub expedited_voting_period: String,
}
#[derive(Deserialize)]
pub struct ParamsResponse {
    pub params: GovParams,
}

#[derive(Deserialize)]
pub struct RegisteredChain {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "network", rename_all = "camelCase")]
pub enum Confirmation {
    Evm {
        receipt: Box<TransactionReceipt>,
    },
    Cosmos {
        height: u64,
        code: u32,
        response: serde_json::Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProtocolIdentity {
    pub address: String,
    pub code_id: String,
    pub checksum: String,
    pub creator: String,
    pub admin: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitialSigners {
    pub identities: Vec<String>,
    pub signers: Vec<(Address, u128)>,
    pub threshold: u128,
    pub nonce: B256,
    pub set_id: String,
    pub hash: B256,
}

pub struct Preflight {
    pub fingerprint: B256,
    pub fingerprint_inputs: serde_json::Value,
    pub protocols: BTreeMap<String, ProtocolIdentity>,
}
