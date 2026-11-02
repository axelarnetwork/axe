use std::path::PathBuf;

use alloy::primitives::{Address, Bytes};
use alloy::rpc::types::TransactionRequest;
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

use crate::types::{ChainKey, Network};

#[derive(Debug, Subcommand)]
pub enum AsgAction {
    /// Preview/deploy ASG through this chain's existing CREATE3 factory
    DeployAsg {
        #[command(flatten)]
        options: Options,
        /// CREATE3 salt key. Must match the selected network's deployment policy
        #[arg(long)]
        salt: Option<String>,
    },
    /// Transfer gateway and ITS ownership to verified ASG, retaining operators
    HandoffAsg {
        #[command(flatten)]
        options: Options,
        /// Existing ASG to verify and record, when absent from config
        #[arg(long)]
        asg: Option<Address>,
    },
}

#[derive(Debug, Args)]
pub struct Options {
    /// Transaction sender. Preview works without a key
    #[arg(long)]
    pub from: Address,
    /// Local chains-config JSON (otherwise CHAINS_CONFIG or sibling checkout)
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// RPC override for this run, never written to deployment config
    #[arg(long, env = "ASG_RPC_URL", hide_env_values = true)]
    pub rpc_url: Option<String>,
    /// ASG artifact (otherwise the deployments checkout's GMP SDK artifact)
    #[arg(long)]
    pub artifact: Option<PathBuf>,
    /// Apply with the action's signing key and verified EVM_GOVERNANCE_OPERATOR_KEY
    #[arg(long)]
    pub apply: bool,
    /// Required together with --apply on mainnet
    #[arg(long)]
    pub confirm_mainnet: bool,
}

pub struct Context {
    pub network: Network,
    pub path: PathBuf,
    pub original: Vec<u8>,
    pub key: ChainKey,
    pub chain_id: u64,
    pub rpc: String,
    pub gateway: Address,
    pub its: Address,
    pub configured_asg: Option<Address>,
    pub create3: Option<Address>,
    pub artifact_path: PathBuf,
    pub settings: Settings,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Settings {
    pub deployer: Address,
    pub salt: &'static str,
    pub legacy_asg: Address,
    pub operator: Address,
    pub delay: u64,
    pub hub_chain_id: &'static str,
}

#[derive(Clone, Copy)]
pub enum TransactionAction {
    Deploy(Address),
    Handoff(Address),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    pub contract_name: String,
    pub bytecode: Bytes,
    pub deployed_bytecode: Bytes,
}

pub struct Deployment {
    pub address: Address,
    pub request: TransactionRequest,
    pub runtime: Bytes,
    pub artifact: Artifact,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Roles {
    pub gateway_owner: Address,
    pub gateway_operator: Address,
    pub its_owner: Address,
    pub sender_is_its_operator: bool,
    pub asg_is_its_operator: bool,
}

#[derive(Serialize, Deserialize)]
pub struct SavedTransaction {
    pub request: TransactionRequest,
    pub raw: Bytes,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RecordedFactory {
    pub create3_deployer: Option<Address>,
}
