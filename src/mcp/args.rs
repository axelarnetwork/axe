//! The arguments each tool accepts.
//!
//! These are the schemas an agent sees. None of them carries a network, a
//! config path, an RPC override, or a key: the network was fixed when the
//! server started (see [`crate::mcp::context::McpContext`]) and everything
//! else comes from the operator environment the server was launched with.
//! A test in the server module fails if a key-bearing field is ever added.

use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;

use crate::cli::{EvmContract, SolProgram};
use crate::commands::load_test::{Protocol, TestType};
use crate::commands::{express_originate, test_express};

pub mod intents;

/// How many recent entries to report when the caller does not say.
pub const DEFAULT_ACTIVITY_LIMIT: usize = 20;

/// Arguments for the block lookup.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BlockArgs {
    /// Block height. Omit for the current head. A height above the head is
    /// predicted from the recent block rate.
    pub number: Option<u64>,
    /// Predict the block at this time, as RFC3339 or unix seconds. Cannot be
    /// combined with a height.
    pub at_time: Option<String>,
}

/// Arguments for the route check.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RouteArgs {
    /// gmp for callContract, its for interchainTransfer, or its-with-data.
    pub protocol: Protocol,
    /// The chain-type pairing, for example sol-to-evm. Omit to infer it from
    /// the two chains' types in the pinned network's config.
    pub route: Option<TestType>,
    /// Source chain axelar id, for example solana.
    pub source_chain: String,
    /// Destination chain axelar id, for example flow.
    pub destination_chain: String,
}

/// Arguments for the Solana activity scan.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SolActivityArgs {
    /// Restrict to one program: gateway, its, gas-service or memo. Omit for all.
    pub program: Option<SolProgram>,
    /// Recent transactions per program. Defaults to 20.
    pub limit: Option<usize>,
}

impl SolActivityArgs {
    pub fn limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_ACTIVITY_LIMIT)
    }
}

/// Arguments for the EVM activity scan.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct EvmActivityArgs {
    /// Chain axelar id, for example avalanche-fuji.
    pub chain: String,
    /// Restrict to one contract: gateway, its or gas-service. Omit for all.
    pub contract: Option<EvmContract>,
    /// Recent events per contract. Defaults to 20.
    pub limit: Option<usize>,
}

impl EvmActivityArgs {
    pub fn limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_ACTIVITY_LIMIT)
    }
}

/// Arguments for the calldata decoder.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CalldataArgs {
    /// Hex calldata, with or without a leading 0x.
    pub calldata: String,
}

/// Arguments for the transaction decoder.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TxArgs {
    /// EVM transaction hash, starting with 0x.
    pub tx_hash: String,
    /// Restrict the search to one chain axelar id. Omit to search all
    /// configured EVM chains.
    pub chain: Option<String>,
}

/// Arguments for the express transfer scan.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExpressScanArgs {
    /// Express-supported chain axelar ids to scan.
    pub chains: Vec<String>,
    /// Recent transfers per chain. Defaults to 5.
    pub recent: Option<usize>,
}

impl ExpressScanArgs {
    pub fn recent(&self) -> usize {
        self.recent.unwrap_or(test_express::DEFAULT_RECENT)
    }
}

/// How long a watch waits before reporting where a transfer had got to, when
/// the caller does not say.
pub const DEFAULT_WAIT_SECS: u64 = 60;

/// The longest a watch will hold a request open, whatever the caller asks for.
///
/// Reimbursement can take half an hour, which is why the CLI waits that long.
/// A tool call holding a request open that long would be cancelled by the
/// client instead, so this waits far less and reports the phase reached. The
/// caller asks again; nothing is lost, because watching spends nothing.
pub const MAX_WAIT_SECS: u64 = 300;

/// Arguments for watching one express transfer.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExpressWatchArgs {
    /// Source transaction hash to watch through both express phases.
    pub source_tx: String,
    /// Seconds to wait for a terminal phase before reporting where it got to.
    /// Defaults to 60, and is capped at 300.
    #[schemars(range(min = 1, max = 300))]
    pub wait_secs: Option<u64>,
}

impl ExpressWatchArgs {
    pub fn wait(&self) -> Duration {
        Duration::from_secs(
            self.wait_secs
                .unwrap_or(DEFAULT_WAIT_SECS)
                .min(MAX_WAIT_SECS),
        )
    }
}

/// Arguments for originating an express transfer.
///
/// The asset, the AxelarApp proxy and the signing key are not here: the first
/// two are fixed by the express registry, and the key comes from the operator
/// environment.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExpressOriginateArgs {
    /// Source chain axelar id. Must be an EVM chain carrying the AxelarApp
    /// proxy, for example avalanche-fuji.
    pub source_chain: String,
    /// Destination chain axelar id.
    pub destination_chain: String,
    /// Express-asset base units at six decimals. Defaults to 5000000, and
    /// must stay inside the express registry's per-chain cap.
    pub amount: Option<String>,
    /// Seconds to watch the transfer for after sending it. Defaults to 60,
    /// and is capped at 300; the transfer is reported either way.
    #[schemars(range(min = 1, max = 300))]
    pub wait_secs: Option<u64>,
}

impl ExpressOriginateArgs {
    pub fn amount(&self) -> String {
        self.amount
            .clone()
            .unwrap_or_else(|| express_originate::DEFAULT_AMOUNT.to_string())
    }

    pub fn wait(&self) -> Duration {
        Duration::from_secs(
            self.wait_secs
                .unwrap_or(DEFAULT_WAIT_SECS)
                .min(MAX_WAIT_SECS),
        )
    }
}

/// Arguments for starting a load test.
///
/// Carries no keys, no RPC overrides and no config path: those come from the
/// operator environment the server was launched with. Nothing an agent sends
/// can substitute a different signer.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct StartLoadTestArgs {
    /// Source chain axelar id, for example solana.
    pub source_chain: String,
    /// Destination chain axelar id, for example flow.
    pub destination_chain: String,
    /// gmp for callContract, its for interchainTransfer, or its-with-data.
    pub protocol: Option<Protocol>,
    /// The chain-type pairing. Omit to let axe infer it from the config.
    pub route: Option<TestType>,
    /// How many transactions to send. Defaults to 1, and must be at least 1.
    #[schemars(range(min = 1))]
    pub num_txs: Option<u64>,
}

impl StartLoadTestArgs {
    pub fn num_txs(&self) -> u64 {
        self.num_txs.unwrap_or(1)
    }
}

/// Arguments for a tool that names one background run.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunArgs {
    /// The run identifier returned by start_load_test, intents_sweep,
    /// intents_traffic or intents_stress.
    pub run_id: String,
}

/// Arguments for a tool that names one chain.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChainArgs {
    /// Chain axelar id, for example solana or avalanche-fuji.
    pub chain: String,
}

/// Arguments for the verifier vote lookup.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct VerifierVotesArgs {
    /// Chain axelar id whose polls to inspect.
    pub chain: String,
    /// The verifier axelar1... address.
    pub verifier: String,
    /// Most recent votes to report. Defaults to 20.
    pub limit: Option<usize>,
}

impl VerifierVotesArgs {
    pub fn limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_ACTIVITY_LIMIT)
    }
}
