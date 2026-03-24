mod cli;
mod commands;
mod cosmos;
mod evm;
mod preflight;
mod solana;
mod state;
mod steps;
pub mod ui;
mod utils;

use clap::Parser;
use eyre::Result;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv_override().ok();

    let cli = cli::Cli::parse();

    match cli.command {
        cli::Commands::Deploy { subcommand } => match subcommand {
            cli::DeployCommands::Init => commands::init::run().await,
            cli::DeployCommands::Status { axelar_id } => commands::status::run(axelar_id),
            cli::DeployCommands::Run {
                axelar_id,
                private_key,
                artifact_path,
                salt,
                proxy_artifact_path,
            } => {
                commands::deploy::run(
                    axelar_id,
                    private_key,
                    artifact_path,
                    salt,
                    proxy_artifact_path,
                )
                .await
            }
            cli::DeployCommands::Reset { axelar_id } => commands::reset::run(axelar_id),
        },
        cli::Commands::Decode { subcommand } => match subcommand {
            cli::DecodeCommands::Calldata { hex } => {
                let joined = hex.join("");
                commands::decode::run(&joined)
            }
            cli::DecodeCommands::Tx {
                txid,
                config,
                chain,
            } => commands::decode_tx::run(&txid, config.as_deref(), chain.as_deref()).await,
        },
        cli::Commands::Test { subcommand } => match subcommand {
            cli::TestCommands::Gmp { axelar_id } => commands::test_gmp::run(axelar_id).await,
            cli::TestCommands::Its { axelar_id } => commands::test_its::run(axelar_id).await,
            cli::TestCommands::LoadTest {
                config,
                test_type,
                num_txs,
                destination_chain,
                source_chain,
                private_key,
                keypair,
                source_rpc,
                destination_rpc,
                payload,
                protocol,
                gas_value,
                token_id,
                tps,
                duration_secs,
                key_cycle,
            } => {
                let resolved = commands::load_test::resolve_from_config(
                    &config,
                    test_type,
                    source_chain,
                    destination_chain,
                    private_key,
                    source_rpc,
                )?;

                // Use --destination-rpc override if provided, otherwise fall back to config.
                let solana_rpc = destination_rpc.unwrap_or(resolved.solana_rpc);

                commands::load_test::run(commands::load_test::LoadTestArgs {
                    config,
                    test_type: resolved.test_type,
                    protocol,
                    destination_chain: resolved.destination_chain,
                    source_chain: resolved.source_chain,
                    solana_rpc,
                    source_rpc: resolved.source_rpc,
                    private_key: resolved.private_key,
                    num_txs,
                    keypair,
                    payload,
                    gas_value,
                    token_id,
                    tps,
                    duration_secs,
                    key_cycle,
                })
                .await
            }
        },
    }
}
