use std::path::PathBuf;

use clap::{Parser, Subcommand};
use eyre::Result;

use crate::commands::load_test::{Protocol, TestType};

#[derive(Parser)]
#[command(name = "axe")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Deploy and manage chain deployments
    Deploy {
        #[command(subcommand)]
        subcommand: DeployCommands,
    },

    /// Test GMP or ITS functionality
    Test {
        #[command(subcommand)]
        subcommand: TestCommands,
    },

    /// Decode EVM calldata or full transactions
    Decode {
        #[command(subcommand)]
        subcommand: DecodeCommands,
    },
}

#[derive(Subcommand)]
pub enum DeployCommands {
    /// Initialize a new chain deployment (reads all config from .env / environment)
    Init,

    /// Show deployment progress
    Status {
        #[arg(long)]
        axelar_id: Option<String>,
    },

    /// Run all pending deployment steps
    Run {
        #[arg(long)]
        axelar_id: Option<String>,
        /// Private key override (auto-resolved per step by default)
        #[arg(long)]
        private_key: Option<String>,
        /// Path to implementation artifact JSON (auto-resolved by default)
        #[arg(long)]
        artifact_path: Option<String>,
        /// Salt for create2 deployments (read from state by default)
        #[arg(long)]
        salt: Option<String>,
        /// Path to proxy artifact JSON (auto-resolved by default)
        #[arg(long)]
        proxy_artifact_path: Option<String>,
    },

    /// Reset all steps to pending and remove all changes from target JSON
    Reset {
        #[arg(long)]
        axelar_id: Option<String>,
    },
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum TestCommands {
    /// Test GMP: send a cross-chain message and relay through the full Amplifier pipeline
    Gmp {
        /// Chain axelar ID (legacy EVM-only mode, uses state file)
        #[arg(long)]
        axelar_id: Option<String>,

        /// Path to chains config JSON (config-based mode, supports EVM + Solana)
        #[arg(long, env = "CHAINS_CONFIG")]
        config: Option<PathBuf>,

        /// Source chain axelar ID
        #[arg(long)]
        source_chain: Option<String>,

        /// Destination chain axelar ID
        #[arg(long)]
        destination_chain: Option<String>,

        /// Cosmos mnemonic for relay transactions
        #[arg(long, env = "MNEMONIC")]
        mnemonic: Option<String>,
    },

    /// Test ITS: deploy interchain token on source, deploy remotely to flow via hub
    Its {
        #[arg(long)]
        axelar_id: Option<String>,
    },

    /// Cross-chain load test (auto-detects chains, RPCs, and test type from config)
    LoadTest {
        /// Path to chains config JSON (e.g. devnet-amplifier.json, testnet.json, mainnet.json)
        #[arg(long, env = "CHAINS_CONFIG")]
        config: PathBuf,

        /// Number of transactions to send
        #[arg(long, default_value = "5")]
        num_txs: u64,

        /// Load test type (auto-detected from source/destination chain types if omitted)
        #[arg(long, value_enum)]
        test_type: Option<TestType>,

        /// Override destination chain axelar ID (auto-detected from config)
        #[arg(long)]
        destination_chain: Option<String>,

        /// Override source chain axelar ID (auto-detected from config)
        #[arg(long)]
        source_chain: Option<String>,

        /// EVM private key for deploying SenderReceiver on destination chain
        #[arg(long, env = "EVM_PRIVATE_KEY")]
        private_key: Option<String>,

        /// Path to Solana keypair JSON file
        #[arg(long, env = "SOLANA_PRIVATE_KEY")]
        keypair: Option<String>,

        /// Override source chain RPC URL (default: from config)
        #[arg(long)]
        source_rpc: Option<String>,

        /// Hex-encoded payload to send (default: random test message)
        #[arg(long)]
        payload: Option<String>,

        /// Protocol: gmp (callContract) or its (interchainTransfer)
        #[arg(long, value_enum, default_value = "gmp")]
        protocol: Protocol,

        /// Gas value to attach for cross-chain gas (in wei, e.g. "10000000000000000")
        #[arg(long)]
        gas_value: Option<String>,

        /// ITS token ID to use (hex, skips token deployment)
        #[arg(long)]
        token_id: Option<String>,

        /// Transactions per second for sustained load test
        #[arg(long)]
        tps: Option<u64>,

        /// Duration in seconds for sustained load test (use with --tps)
        #[arg(long)]
        duration_secs: Option<u64>,

        /// Key cycle interval in seconds (default: 3). Each signing key waits this
        /// many seconds before reuse. pool_size = tps × key_cycle. Higher values
        /// use more wallets, reducing per-address mempool pressure.
        #[arg(long, default_value = "3")]
        key_cycle: u64,
    },
}

#[derive(Subcommand)]
pub enum DecodeCommands {
    /// Decode raw hex calldata (ITS, Gateway, Factory)
    Calldata {
        /// Hex-encoded calldata (with or without 0x prefix, whitespace is stripped)
        #[arg(trailing_var_arg = true, num_args = 1..)]
        hex: Vec<String>,
    },

    /// Fetch and decode a full EVM transaction (calldata + logs)
    Tx {
        /// Transaction hash (0x...)
        txid: String,

        /// Path to chains config JSON (auto-discovered from sibling axelar-contract-deployments repo)
        #[arg(long, env = "CHAINS_CONFIG")]
        config: Option<PathBuf>,

        /// Chain axelar ID (skip RPC brute-forcing)
        #[arg(long)]
        chain: Option<String>,
    },
}

pub fn resolve_axelar_id(opt: Option<String>) -> Result<String> {
    opt.or_else(|| std::env::var("CHAIN").ok())
        .ok_or_else(|| eyre::eyre!("--axelar-id not provided and CHAIN env var not set"))
}
