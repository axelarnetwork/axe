mod evm_sender;
mod gmp;
mod helpers;
mod its_evm_to_sol;
mod its_evm_to_sol_with_data;
mod its_evm_to_stellar;
mod its_evm_to_xrpl;
mod its_sol_to_evm;
mod its_stellar_to_evm;
mod its_stellar_to_sol;
mod its_sui_to_evm;
mod its_xrpl_to_evm;
mod keypairs;
pub mod metrics;
mod resolve;
mod sol_sender;
mod stellar_sender;
mod sustained;
mod verify;
mod xrpl_sender;

// Re-exports for callers outside the load_test module:
// - `ensure_sender_receiver_on_evm_chain` is used by `commands::test_gmp`
//   (the `test_gmp --config` flow deploys / reuses a SenderReceiver on the
//   destination EVM chain).
// - `make_executable_payload` / `memo_program_id` are used by
//   `commands::test_gmp::source` to build the Solana-side memo payload.
// - `resolve_from_config` is used by `main.rs` to resolve a chains-config
//   JSON into a `ResolvedConfig` before dispatching to `run`.
pub(crate) use evm_sender::{make_executable_payload, memo_program_id};
pub(crate) use helpers::ensure_sender_receiver_on_evm_chain;
pub(crate) use resolve::resolve_from_config;

// Re-export helpers/resolve names through `load_test` so the per-pair
// modules (its_*.rs, gmp.rs, the *_sender modules) can keep calling them
// as `super::name`. Each entry is `pub(super)`, restricting reach to
// `load_test` itself — exactly the scope its siblings sit in.
pub(super) use helpers::{
    axelar_id_for_chain, check_evm_balance, deploy_or_reuse_sender_receiver,
    deploy_sender_receiver, ensure_evm_contract_deployed, ensure_sender_receiver,
    finalize_sui_dest_run, finish_report, load_stellar_main_wallet, load_sui_main_wallet,
    read_stellar_contract_address, read_stellar_network_type, read_stellar_token_address,
    resolve_sui_axe_token, sui_dest_lookup, validate_evm_rpc, validate_solana_rpc,
};
pub(super) use resolve::{
    compiled_network, detect_network_from_config, read_cache, read_its_cache, save_cache,
    save_its_cache,
};

use std::path::PathBuf;
use std::time::Instant;

use eyre::Result;

use crate::config::ChainsConfig;
use crate::ui;

/// Load test type (extensible for future directions).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum TestType {
    /// Solana -> EVM (GMP, ITS)
    SolToEvm,
    /// EVM -> Solana (GMP, ITS, ITS-with-data)
    EvmToSol,
    /// EVM -> EVM (GMP)
    EvmToEvm,
    /// Solana -> Solana (GMP)
    SolToSol,
    /// XRPL -> EVM (ITS, canonical XRP)
    XrplToEvm,
    /// EVM -> XRPL (ITS, canonical XRP)
    EvmToXrpl,
    /// Stellar -> EVM (GMP, ITS)
    StellarToEvm,
    /// EVM -> Stellar (GMP, ITS)
    EvmToStellar,
    /// Stellar -> Solana (GMP only — Stellar ITS testnet does not yet trust "solana"
    /// as a destination chain; ITS will fail with Contract Error #7
    /// (UntrustedChain) until the ITS owner runs `add-trusted-chains solana`.)
    StellarToSol,
    /// Solana -> Stellar (GMP)
    SolToStellar,
    /// Sui -> EVM (GMP). ITS variant forthcoming.
    SuiToEvm,
    /// EVM -> Sui (GMP + ITS scaffolded; runs are stubbed pending Sui
    /// destination verifier wiring in poll_pipeline).
    EvmToSui,
    /// Sui -> Solana (GMP + ITS scaffolded).
    SuiToSol,
    /// Solana -> Sui (GMP + ITS scaffolded).
    SolToSui,
    /// Sui -> Stellar (GMP + ITS scaffolded).
    SuiToStellar,
    /// Stellar -> Sui (GMP + ITS scaffolded).
    StellarToSui,
    /// Sui -> XRPL (ITS only — XRPL has no GMP).
    SuiToXrpl,
    /// XRPL -> Sui (ITS only).
    XrplToSui,
}

impl std::fmt::Display for TestType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TestType::SolToEvm => write!(f, "sol-to-evm"),
            TestType::EvmToSol => write!(f, "evm-to-sol"),
            TestType::EvmToEvm => write!(f, "evm-to-evm"),
            TestType::SolToSol => write!(f, "sol-to-sol"),
            TestType::XrplToEvm => write!(f, "xrpl-to-evm"),
            TestType::EvmToXrpl => write!(f, "evm-to-xrpl"),
            TestType::StellarToEvm => write!(f, "stellar-to-evm"),
            TestType::EvmToStellar => write!(f, "evm-to-stellar"),
            TestType::StellarToSol => write!(f, "stellar-to-sol"),
            TestType::SolToStellar => write!(f, "sol-to-stellar"),
            TestType::SuiToEvm => write!(f, "sui-to-evm"),
            TestType::EvmToSui => write!(f, "evm-to-sui"),
            TestType::SuiToSol => write!(f, "sui-to-sol"),
            TestType::SolToSui => write!(f, "sol-to-sui"),
            TestType::SuiToStellar => write!(f, "sui-to-stellar"),
            TestType::StellarToSui => write!(f, "stellar-to-sui"),
            TestType::SuiToXrpl => write!(f, "sui-to-xrpl"),
            TestType::XrplToSui => write!(f, "xrpl-to-sui"),
        }
    }
}

/// Protocol: GMP (callContract), ITS (interchainTransfer), or ITS with data
/// (interchainTransfer that triggers a contract call on the destination).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Protocol {
    #[default]
    Gmp,
    Its,
    /// ITS interchainTransfer with data — sends tokens AND calls the memo
    /// program on the Solana destination chain.
    ItsWithData,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Protocol::Gmp => write!(f, "gmp"),
            Protocol::Its => write!(f, "its"),
            Protocol::ItsWithData => write!(f, "its-with-data"),
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
    /// Sui Move type tag for ITS coin (e.g. `0x...::token::TOKEN`). Used by
    /// Sui-source ITS runs; resolved via dev-inspect when omitted.
    pub coin_type: Option<String>,
    pub tps: Option<u64>,
    pub duration_secs: Option<u64>,
    pub key_cycle: u64,
    /// Number of extra accounts to add to ITS-with-data payloads.
    /// The first extra account is a valid ATA for the ITS token mint;
    /// remaining accounts are random pubkeys. Useful for testing ALT paths.
    pub extra_accounts: u32,
}

pub async fn run(args: LoadTestArgs) -> Result<()> {
    // Check for network mismatch between compiled binary and config
    if let Some(target_network) = detect_network_from_config(&args.config) {
        let compiled = compiled_network();
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

    // Block consensus chains that have no VotingVerifier — we can't verify them.
    // XRPL uses `XrplVotingVerifier` (not `VotingVerifier`), so we also accept
    // that as evidence of a verifiable source.
    let src = &args.source_chain;
    let cfg = ChainsConfig::load(&args.config)?;
    let has_standard_vv = cfg.axelar.contract_address("VotingVerifier", src).is_ok();
    let has_xrpl_vv = cfg
        .axelar
        .contract_address("XrplVotingVerifier", src)
        .is_ok();
    // Stellar shares the `VotingVerifier` contract name in the config, so the
    // standard check above already covers it; this branch is just documentation.
    if !has_standard_vv && !has_xrpl_vv {
        eyre::bail!(
            "source chain '{src}' has no VotingVerifier (or XrplVotingVerifier) in the config. \
             Load test verification requires an Amplifier chain with a voting verifier."
        );
    }

    match (args.protocol, args.test_type) {
        (Protocol::Gmp, TestType::SolToEvm) => gmp::run_sol_to_evm(args, run_start).await,
        (Protocol::Gmp, TestType::EvmToSol) => gmp::run_evm_to_sol(args, run_start).await,
        (Protocol::Gmp, TestType::EvmToEvm) => gmp::run_evm_to_evm(args, run_start).await,
        (Protocol::Gmp, TestType::SolToSol) => gmp::run_sol_to_sol(args, run_start).await,
        (Protocol::Gmp, TestType::XrplToEvm | TestType::EvmToXrpl) => {
            eyre::bail!(
                "GMP {}->{} is not yet supported for XRPL. XRPL has no executable layer, \
                 so GMP in either direction is not applicable; use --protocol its instead.",
                args.source_chain,
                args.destination_chain
            )
        }
        (Protocol::Gmp, TestType::StellarToEvm) => gmp::run_stellar_to_evm(args, run_start).await,
        (Protocol::Gmp, TestType::EvmToStellar) => gmp::run_evm_to_stellar(args, run_start).await,
        (Protocol::Gmp, TestType::StellarToSol) => gmp::run_stellar_to_sol(args, run_start).await,
        (Protocol::Gmp, TestType::SolToStellar) => gmp::run_sol_to_stellar(args, run_start).await,
        (Protocol::Its, TestType::StellarToEvm) => its_stellar_to_evm::run(args, run_start).await,
        (Protocol::Its, TestType::EvmToStellar) => its_evm_to_stellar::run(args, run_start).await,
        // Stellar -> Solana ITS: code is in place, but the destination chain
        // must be in the Stellar ITS contract's trusted-chains list. On
        // testnet today "solana" is not registered, so the source-side
        // simulation reverts with Contract Error #7. The runner will surface
        // that clearly. We leave it dispatched so the run becomes possible
        // automatically once the trusted-chain config is updated upstream.
        (Protocol::Its, TestType::StellarToSol) => its_stellar_to_sol::run(args, run_start).await,
        (Protocol::Its, TestType::SolToStellar) => {
            eyre::bail!(
                "ITS sol -> stellar is not implemented yet. Use --protocol gmp for this pair."
            )
        }
        (Protocol::Its, TestType::EvmToSol) => its_evm_to_sol::run(args, run_start).await,
        (Protocol::Its, TestType::SolToEvm) => its_sol_to_evm::run(args, run_start).await,
        (Protocol::Its, TestType::XrplToEvm) => its_xrpl_to_evm::run(args, run_start).await,
        (Protocol::Its, TestType::EvmToXrpl) => its_evm_to_xrpl::run(args, run_start).await,
        // (kept for clarity — the dispatch line above already wires it up)
        (Protocol::Its, TestType::EvmToEvm | TestType::SolToSol) => {
            eyre::bail!(
                "ITS {}->{} is not yet supported",
                args.source_chain,
                args.destination_chain
            )
        }
        (Protocol::Gmp, TestType::SuiToEvm) => gmp::run_sui_to_evm(args, run_start).await,
        (Protocol::ItsWithData, TestType::EvmToSol) => {
            its_evm_to_sol_with_data::run(args, run_start).await
        }
        (Protocol::ItsWithData, _) => {
            eyre::bail!("its-with-data only supports evm-to-sol currently")
        }
        // Sui as destination — Sui events-based verifier is now wired in
        // verify.rs. EVM -> Sui GMP runs end-to-end. ITS to Sui still
        // needs the receive-side coin type plumbing.
        (Protocol::Gmp, TestType::EvmToSui) => gmp::run_evm_to_sui(args, run_start).await,
        (Protocol::Its, TestType::EvmToSui) => {
            eyre::bail!(
                "evm -> sui ITS still needs Sui-side `interchain_token_service::receive_interchain_transfer<T>` \
                 type-tag resolution and a registered AXE coin on Sui. GMP (--protocol gmp) works."
            )
        }
        (Protocol::Gmp, TestType::SolToSui) => gmp::run_sol_to_sui(args, run_start).await,
        (Protocol::Its, TestType::SolToSui) => {
            eyre::bail!(
                "sol -> sui ITS still needs the Sui-side `interchain_token_service::receive_interchain_transfer<T>` \
                 type-tag resolution. GMP works (--protocol gmp)."
            )
        }
        (Protocol::Gmp, TestType::StellarToSui) => gmp::run_stellar_to_sui(args, run_start).await,
        (Protocol::Its, TestType::StellarToSui) => {
            eyre::bail!(
                "stellar -> sui ITS needs Sui-side AXE coin registration + receive helper. GMP works \
                 (--protocol gmp), pending Stellar ITS adding 'sui' as trusted-chain upstream."
            )
        }
        (_, TestType::XrplToSui) => {
            eyre::bail!(
                "xrpl -> sui ITS needs the Sui destination verifier plus a registered AXE/XRP \
                 token on Sui ITS. Not yet implemented."
            )
        }
        // Sui-source ITS. We don't auto-deploy a fresh AXE token on Sui
        // (Move package publish from Rust is impractical), so the user must
        // pre-register a token via axelar-contract-deployments/sui/its.js
        // and pass `--token-id`. `--coin-type` resolves automatically via
        // dev-inspect when omitted.
        (Protocol::Its, TestType::SuiToEvm) => its_sui_to_evm::run(args, run_start).await,
        (Protocol::Its, TestType::SuiToSol)
        | (Protocol::Its, TestType::SuiToStellar)
        | (Protocol::Its, TestType::SuiToXrpl) => {
            eyre::bail!(
                "sui -> {} ITS not yet wired (only sui -> evm is). Source-side PTB construction is identical, \
                 only the destination verifier differs — follow the its_sui_to_evm.rs pattern to add it.",
                args.destination_chain
            )
        }
        // Sui-source GMP variants other than SuiToEvm — same upstream voter
        // gap as SuiToEvm GMP, so just route to a friendly bail.
        (Protocol::Gmp, TestType::SuiToSol)
        | (Protocol::Gmp, TestType::SuiToStellar)
        | (Protocol::Gmp, TestType::SuiToXrpl) => {
            eyre::bail!(
                "sui -> {} GMP not implemented (and Example::gmp::send_call voting is upstream-stale on testnet). \
                 Use ITS via axelar-contract-deployments while the voter set catches up.",
                args.destination_chain
            )
        }
    }
}
