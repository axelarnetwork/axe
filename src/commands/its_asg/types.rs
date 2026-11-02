use std::path::PathBuf;

use alloy::primitives::{Address, Bytes};
use alloy::rpc::types::TransactionRequest;
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

use crate::types::{ChainKey, Network};

#[derive(Debug, Subcommand)]
pub enum AsgAction {
    /// Preview/deploy ASG with this network's governance settings
    DeployAsg {
        #[command(flatten)]
        options: Options,
        /// CREATE3 salt key, encoded like axelar-contract-deployments
        #[arg(long, default_value = "v6.1.0")]
        salt: String,
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
    /// ASG artifact (otherwise the deployments checkout's GMP SDK artifact)
    #[arg(long)]
    pub artifact: Option<PathBuf>,
    /// Apply with ASG_PRIVATE_KEY and verified EVM_GOVERNANCE_OPERATOR_KEY
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
    pub create3: Option<Address>,
    pub configured_asg: Option<Address>,
    pub artifact_path: PathBuf,
    pub settings: Settings,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Settings {
    pub operator: Address,
    pub delay: u64,
    pub hub_chain_id: &'static str,
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
