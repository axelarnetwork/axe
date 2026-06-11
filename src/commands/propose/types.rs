//! Types for `axe propose` — submit an AxelarServiceGovernance proposal to an
//! edge chain's ASG via the Axelar hub (gov proposal → AxelarnetGateway GMP).

use clap::{Args, ValueEnum};

/// `axe propose <network> <chain>` — submit an AxelarServiceGovernance proposal.
#[derive(Debug, Args)]
pub struct ProposeArgs {
    /// Network: testnet | stagenet | devnet-amplifier | mainnet.
    pub network: String,
    /// Edge chain key the ASG lives on (e.g. `hedera`).
    pub chain: String,
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
    pub multisig_prover: String,
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
