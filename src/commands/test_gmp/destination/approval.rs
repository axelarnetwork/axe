use std::time::Duration;

use alloy::{providers::Provider, rpc::types::TransactionReceipt};
use eyre::Result;

use super::EvmExecutionRequest;
use crate::{evm::AxelarAmplifierGateway, ui};

#[cfg(test)]
mod tests;

const TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub(super) async fn wait<P: Provider>(
    request: &EvmExecutionRequest<'_, P>,
    receipt: &TransactionReceipt,
) -> Result<()> {
    wait_with_timeout(request, receipt, TIMEOUT, POLL_INTERVAL).await
}

async fn wait_with_timeout<P: Provider>(
    request: &EvmExecutionRequest<'_, P>,
    receipt: &TransactionReceipt,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<()> {
    let tx_hash = receipt.transaction_hash;
    eyre::ensure!(
        receipt.status(),
        "gateway proof transaction {tx_hash} reverted"
    );
    ui::info(&format!(
        "Waiting up to {} for gateway approval to appear in RPC reads (checking every {}).",
        ui::format_duration(timeout),
        ui::format_duration(poll_interval),
    ));
    ui::info("The proof transaction is already submitted. These checks do not resend it.");
    let gateway = AxelarAmplifierGateway::new(request.gateway, request.provider);
    let spinner = ui::wait_spinner("checking gateway approval...");
    let mut last_observation = "no response before deadline".to_string();
    let result = tokio::time::timeout(timeout, async {
        loop {
            match gateway
                .isMessageApproved(
                    request.source_chain.to_string(),
                    request.message_id.to_string(),
                    request.source_address.to_string(),
                    request.sender_receiver,
                    request.payload_hash,
                )
                .call()
                .await
            {
                Ok(true) => return,
                Ok(false) => {
                    last_observation = "isMessageApproved returned false".to_string();
                }
                Err(error) => {
                    last_observation = ui::scrub_urls(&format!("approval query failed: {error}"));
                }
            }
            spinner.set_message(format!("{last_observation}; retrying read..."));
            tokio::time::sleep(poll_interval).await;
        }
    })
    .await;
    spinner.finish_and_clear();
    result.map_err(|_| {
        eyre::eyre!(
            "Could not confirm gateway approval within {} for message {}. Proof transaction: {tx_hash}. Last observation: {last_observation}. Axe has not sent the receiver execution. Inspect this transaction and message before starting another test; a false or failed RPC read does not prove the transaction failed.",
            ui::format_duration(timeout), request.message_id,
        )
    })?;
    ui::kv("isMessageApproved", "true");
    Ok(())
}
