//! Load-test command. The dispatcher [`run`] picks one of the per-pair
//! orchestrators (in [`gmp`] for GMP-protocol pairs, or [`its_evm_to_sol`] /
//! [`its_sol_to_evm`] for ITS) based on `(protocol, test_type)`. Config
//! resolution + caches live in [`resolve`]; shared helpers in [`helpers`].

pub mod evm_sender;
mod gmp;
mod helpers;
pub mod its_evm_to_sol;
pub mod its_sol_to_evm;
pub mod keypairs;
pub mod metrics;
mod resolve;
pub mod sol_sender;
mod sustained;
mod verify;

use std::path::PathBuf;
use std::time::Instant;

use eyre::Result;

pub use resolve::resolve_from_config;

use crate::cosmos::read_axelar_contract_field;
use crate::ui;

/// Load test type (extensible for future directions).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum TestType {
    /// Solana -> EVM cross-chain load test
    SolToEvm,
    /// EVM -> Solana cross-chain load test
    EvmToSol,
    /// EVM -> EVM cross-chain load test
    EvmToEvm,
    /// Solana -> Solana cross-chain load test
    SolToSol,
}

impl std::fmt::Display for TestType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TestType::SolToEvm => write!(f, "sol-to-evm"),
            TestType::EvmToSol => write!(f, "evm-to-sol"),
            TestType::EvmToEvm => write!(f, "evm-to-evm"),
            TestType::SolToSol => write!(f, "sol-to-sol"),
        }
    }
}

/// Protocol: GMP (callContract) or ITS (interchainTransfer).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Protocol {
    #[default]
    Gmp,
    Its,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Protocol::Gmp => write!(f, "gmp"),
            Protocol::Its => write!(f, "its"),
        }
    }
}

/// CLI arguments for the load test command.
pub struct LoadTestArgs {
    pub config: PathBuf,
    pub test_type: TestType,
    pub protocol: Protocol,
    pub destination_chain: String,
    pub source_chain: String,
    /// The `axelarId` for the source chain (used for Cosmos-side verification).
    pub source_axelar_id: String,
    /// The `axelarId` for the destination chain (used for Cosmos-side verification).
    pub destination_axelar_id: String,
    pub source_rpc: String,
    pub destination_rpc: String,
    pub private_key: Option<String>,
    pub num_txs: u64,
    pub keypair: Option<String>,
    pub payload: Option<String>,
    pub gas_value: Option<String>,
    pub token_id: Option<String>,
    pub tps: Option<u64>,
    pub duration_secs: Option<u64>,
    pub key_cycle: u64,
}

pub async fn run(args: LoadTestArgs) -> Result<()> {
    // Check for network mismatch between compiled binary and config
    if let Some(target_network) = resolve::detect_network_from_config(&args.config) {
        let compiled = crate::types::Network::from_features();
        if compiled != target_network {
            eyre::bail!(
                "binary was compiled for '{compiled}' but config targets '{target_network}'. \
                 Rebuild with:\n  cargo build --release --features {target_network} --no-default-features"
            );
        }
    }

    let run_start = Instant::now();

    ui::section(&format!(
        "Load Test ({}/{}): {} -> {}",
        args.protocol, args.test_type, args.source_chain, args.destination_chain
    ));

    // Block consensus chains that have no VotingVerifier — we can't verify them
    let src = &args.source_chain;
    let has_source_vv = read_axelar_contract_field(
        &args.config,
        &format!("/axelar/contracts/VotingVerifier/{src}/address"),
    )
    .is_ok();
    if !has_source_vv {
        eyre::bail!(
            "source chain '{src}' has no VotingVerifier in the config (consensus chain). \
             Load test verification requires an Amplifier chain with a VotingVerifier."
        );
    }

    match (args.protocol, args.test_type) {
        (Protocol::Gmp, TestType::SolToEvm) => gmp::run_sol_to_evm(args, run_start).await,
        (Protocol::Gmp, TestType::EvmToSol) => gmp::run_evm_to_sol(args, run_start).await,
        (Protocol::Gmp, TestType::EvmToEvm) => gmp::run_evm_to_evm(args, run_start).await,
        (Protocol::Gmp, TestType::SolToSol) => gmp::run_sol_to_sol(args, run_start).await,
        (Protocol::Its, TestType::EvmToSol) => its_evm_to_sol::run(args, run_start).await,
        (Protocol::Its, TestType::SolToEvm) => its_sol_to_evm::run(args, run_start).await,
        (Protocol::Its, TestType::EvmToEvm | TestType::SolToSol) => {
            eyre::bail!(
                "ITS {}->{} is not yet supported",
                args.source_chain,
                args.destination_chain
            )
        }
    }
}
