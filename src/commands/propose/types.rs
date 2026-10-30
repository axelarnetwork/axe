//! Types for `axe propose` — submit an AxelarServiceGovernance proposal to an
//! edge chain's ASG via the Axelar hub (gov proposal → AxelarnetGateway GMP).

use alloy::primitives::{Address, Bytes};
use clap::{Args, ValueEnum};
use eyre::Result;

/// A complete governance call and its encoded payload, ready for relaying.
pub struct RelayPlan {
    pub ptype: ProposalType,
    pub target: Address,
    pub calldata: Bytes,
    pub payload: Bytes,
}

/// One governance call to one edge chain's ASG: the resolved chain config, the
/// verified ASG, and the inner `(target, calldata)` the ASG will run.
pub struct EdgeCall {
    /// JSON key of the chain in the chains config (e.g. `hedera`).
    pub chain: String,
    pub cfg: ResolvedConfig,
    pub asg: super::helpers::AsgInfo,
    pub target: Address,
    pub calldata: Bytes,
}

impl EdgeCall {
    /// Pair this call with its encoded governance payload for relaying.
    pub fn relay_plan(&self, ptype: ProposalType, payload: Bytes) -> RelayPlan {
        RelayPlan {
            ptype,
            target: self.target,
            calldata: self.calldata.clone(),
            payload,
        }
    }
}

/// Everything one gov proposal carries: one ASG command per edge chain, all of
/// the same type and action. Hub-side parameters (LCD, gov module, deposits)
/// are shared by every call and read from the first one.
pub struct ProposalPlan {
    pub ptype: ProposalType,
    pub action_label: String,
    pub calls: Vec<EdgeCall>,
}

impl ProposalPlan {
    /// A plan always carries at least one call, and `hub()` relies on that.
    pub fn new(ptype: ProposalType, action_label: String, calls: Vec<EdgeCall>) -> Result<Self> {
        eyre::ensure!(
            !calls.is_empty(),
            "a proposal plan needs at least one edge call"
        );
        Ok(Self {
            ptype,
            action_label,
            calls,
        })
    }

    /// Hub-side config, identical across calls since they come from one chains file.
    pub fn hub(&self) -> &ResolvedConfig {
        &self.calls[0].cfg
    }

    pub fn edge_ids(&self) -> Vec<&str> {
        self.calls
            .iter()
            .map(|call| call.cfg.edge_axelar_id.as_str())
            .collect()
    }
}

/// A terminal governance result, separate from errors while reading its state.
pub enum ProposalOutcome {
    Passed,
    Rejected(eyre::Report),
    Failed(eyre::Report),
}

/// `axe propose <network> <chain>` — submit an AxelarServiceGovernance proposal.
#[derive(Debug, Args)]
pub struct ProposeArgs {
    /// Network: testnet | stagenet | devnet-amplifier | mainnet.
    pub network: crate::types::Network,
    /// Edge chain key the ASG lives on (e.g. `hedera`). Omit with `--chains`.
    pub chain: Option<String>,
    /// Edge chains for one multi-call proposal, with `--op`: `all` (every live
    /// ITS Hub EVM chain whose ITS the ASG owns) or comma-separated chain keys.
    #[arg(long, value_delimiter = ',', conflicts_with = "chain")]
    pub chains: Vec<String>,
    /// Chain keys to leave out of `--chains all`.
    #[arg(long, value_delimiter = ',')]
    pub skip: Vec<String>,
    /// Catalog operation to propose. Omit to pass a raw call with `--calldata`/`--target`.
    #[arg(long, value_enum)]
    pub op: Option<Operation>,
    /// Raw calldata hex for the inner call (requires `--target`).
    #[arg(long)]
    pub calldata: Option<String>,
    /// Raw target address for `--calldata`.
    #[arg(long)]
    pub target: Option<String>,
    /// ITS chain name, for `set-trusted`/`remove-trusted`.
    #[arg(long)]
    pub its_chain: Option<String>,
    /// Proposal type: `operator` (fast-path, default) or `timelock`.
    #[arg(long = "type", value_enum, default_value = "operator")]
    pub proposal_type: ProposalType,
    /// After the proposal passes, relay it to the edge chain and execute it.
    #[arg(long)]
    pub relay: bool,
    /// Resume this existing proposal without submitting a new proposal.
    #[arg(long, conflicts_with = "new_proposal")]
    pub proposal_id: Option<u64>,
    /// Intentionally submit again instead of recovering a matching proposal.
    #[arg(long)]
    pub new_proposal: bool,
    /// Submit as a standard (non-expedited) gov proposal.
    #[arg(long)]
    pub standard: bool,
    /// Time-lock `eta` override (unix seconds). Defaults to now + ASG delay + buffer.
    #[arg(long)]
    pub eta: Option<u64>,
    /// Required to run against mainnet (giga-gated).
    #[arg(long)]
    pub confirm_mainnet: bool,
    /// Skip the confirmation prompt before submitting.
    #[arg(long = "y", alias = "yes")]
    pub yes: bool,
}

/// Which Service-Governance command the proposal carries. The number is the
/// ASG `Commands` enum discriminant encoded as the first ABI word.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ProposalType {
    /// `ApproveOperatorProposal` (2) — gov approves, the ASG operator executes
    /// immediately (the fast path). This is the default.
    Operator,
    /// `ScheduleTimeLockProposal` (0) — gov schedules; anyone may execute once
    /// the time-lock `eta` has passed.
    Timelock,
}

impl ProposalType {
    /// The ASG `Commands` discriminant for this proposal type.
    pub const fn command(self) -> u8 {
        match self {
            Self::Operator => 2,
            Self::Timelock => 0,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Operator => "operator (ApproveOperatorProposal)",
            Self::Timelock => "timelock (ScheduleTimeLockProposal)",
        }
    }
}

/// A known operation the ASG can perform on a target contract. Each maps to a
/// `(target, calldata)` pair resolved against the chain config. Anything not in
/// this catalog can be sent as a raw `--target` + `--calldata` call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Operation {
    /// `gateway.setPauseStatus(true)`
    Pause,
    /// `gateway.setPauseStatus(false)`
    Unpause,
    /// `its.setTrustedChain(<--its-chain>)`
    SetTrusted,
    /// `its.removeTrustedChain(<--its-chain>)`
    RemoveTrusted,
    /// `its.setPauseStatus(true)` (only meaningful once the ASG owns ITS)
    ItsPause,
}

impl Operation {
    /// Which deployed contract this operation targets on the edge chain.
    pub const fn target_contract(self) -> TargetContract {
        match self {
            Self::Pause | Self::Unpause => TargetContract::Gateway,
            Self::SetTrusted | Self::RemoveTrusted | Self::ItsPause => TargetContract::Its,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Pause => "pause gateway",
            Self::Unpause => "unpause gateway",
            Self::SetTrusted => "ITS set-trusted-chain",
            Self::RemoveTrusted => "ITS remove-trusted-chain",
            Self::ItsPause => "ITS pause",
        }
    }
}

/// Everything the proposal needs, pulled out of the chain config once so the
/// orchestrator and helpers don't re-walk the JSON. Owned (no lifetimes) per
/// the project's bundle-struct convention.
#[derive(Clone, Debug)]
pub struct ResolvedConfig {
    /// Edge chain's `axelarId` — the `destination_chain` for the GMP call.
    pub edge_axelar_id: String,
    pub asg_address: String,
    pub gateway_address: String,
    pub its_address: Option<String>,
    pub edge_rpc: String,
    /// Edge chain's `MultisigProver` on the hub — builds the relay proof.
    /// Absent on consensus chains, whose approvals Axelar validators produce.
    pub multisig_prover: Option<String>,
    /// Axelar Tendermint RPC — for `block_results` (finding the gov GMP message).
    pub axelar_rpc: String,
    /// Cosmos `AxelarnetGateway` contract — the GMP entrypoint on the hub.
    pub axelarnet_gateway: String,
    /// Gov module bech32 address — submits the inner `call_contract` and must
    /// equal the ASG's `governanceAddress`.
    pub gov_module: String,
    pub lcd: String,
    pub chain_id: String,
    pub fee_denom: String,
    pub gas_price: f64,
    /// gov deposit (base-denom micro-units) for a standard proposal.
    pub deposit_amount: String,
    /// gov deposit for an expedited proposal.
    pub expedited_deposit_amount: String,
}

impl ResolvedConfig {
    pub fn route(&self) -> EdgeRoute {
        if self.multisig_prover.is_some() {
            EdgeRoute::Amplifier
        } else {
            EdgeRoute::Consensus
        }
    }
}

/// How a governance GMP reaches the edge gateway.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeRoute {
    /// Amplifier chain: axe builds the proof on the MultisigProver and submits it.
    Amplifier,
    /// Consensus chain: Axelar validators approve on the legacy gateway, and axe
    /// waits for that approval and then executes.
    Consensus,
}

impl EdgeRoute {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Amplifier => "amplifier",
            Self::Consensus => "consensus",
        }
    }
}

/// The deployed contract an operation targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetContract {
    Gateway,
    Its,
}

impl TargetContract {
    /// The `chains.<chain>.contracts.<name>` key.
    pub const fn config_key(self) -> &'static str {
        match self {
            Self::Gateway => "AxelarGateway",
            Self::Its => "InterchainTokenService",
        }
    }
}

/// Governance GMP emitted by the Axelar gov module.
pub struct GovMessage {
    pub message_id: String,
    pub source_chain: String,
    pub source_address: String,
}
