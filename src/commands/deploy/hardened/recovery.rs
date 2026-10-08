use super::{
    cosmos_recovery, journal, session, storage,
    types::{Confirmation, Transaction},
};
use crate::{
    commands::deploy::DeployContext,
    state::{StepStatus, next_pending_step},
    ui,
};
use eyre::Result;
use sha2::Digest;

pub fn validate_options(actions: &[(String, Transaction)]) -> Result<()> {
    let session = session::current()?;
    for key in [&session.options.bump_fees, &session.options.retry_failed]
        .into_iter()
        .flatten()
    {
        let (_, saved) = actions.iter().find(|(name, _)| name == key)
            .ok_or_else(|| eyre::eyre!("unknown recovery action {key}; remove the flag if that attempt was already retired and resume normally"))?;
        if session.options.retry_failed.as_ref() == Some(key)
            && session.options.retry_gas_limit.is_some()
        {
            eyre::ensure!(
                matches!(saved, Transaction::Evm { .. }),
                "--retry-gas-limit only applies to EVM actions"
            );
        }
        if session.options.bump_fees.as_ref() == Some(key) {
            eyre::ensure!(
                matches!(saved, Transaction::Cosmos { .. }) == session.options.cosmos_fee.is_some(),
                "Cosmos fee replacement requires --cosmos-fee <total-base-units>; EVM replacements must omit it"
            );
        }
    }
    Ok(())
}

pub async fn retry_failed(
    ctx: &DeployContext,
    key: &str,
    saved: &Transaction,
    lcd: &str,
) -> Result<bool> {
    let session = session::current()?;
    if session.options.retry_failed.as_deref() != Some(key) {
        return Ok(false);
    }
    validate_progress(ctx, key)?;
    let hash = match saved {
        Transaction::Evm { .. } => super::evm_retry::prove(ctx, key, saved).await?,
        Transaction::Cosmos { .. } => {
            eyre::ensure!(
                matches!(
                    key,
                    "WaitForVerifierSet/cosmos"
                        | "AddRewards/cosmos"
                        | "InstantiateChainContracts/cosmos"
                        | "RegisterDeployment/cosmos"
                        | "RegisterItsOnHub/cosmos"
                ),
                "unsupported Cosmos retry action"
            );
            // Read-only: recovery must never broadcast an unknown transaction just to prove failure.
            let _ = cosmos_recovery::confirmed(lcd, saved, key).await;
            let hash = failed_attempt(key, saved).await?;
            if let Transaction::Cosmos {
                sender, sequence, ..
            } = saved
            {
                let (_, current) = crate::cosmos::rpc::lcd_query_account(lcd, sender).await?;
                eyre::ensure!(
                    current > *sequence,
                    "failed Cosmos transaction did not consume its sequence; keep the journal and reconcile the account before retiring this attempt"
                );
            }
            hash
        }
    };
    ui::tx_hash("proven failed transaction", &hash);
    ui::info(
        "The failed transaction's execution did not commit; fees remain spent. Retire it and resume the pending step with a new nonce/sequence and a separate transaction approval. A failed governance vote is never resubmitted by this option.",
    );
    if let Some(limit) = session.options.retry_gas_limit {
        ui::kv("new EVM gas limit", &limit.to_string());
    }
    if !ui::confirm(&format!("Retire the failed attempt of {key}? ")).await {
        return Err(session::pause("retry declined"));
    }
    retire(key, session.options.retry_gas_limit).await?;
    // End this run so preflight recalculates deposits and rewards for the new attempt.
    Err(session::pause(
        "Failed attempt retired. Resume without --retry-failed/--retry-gas-limit; the saved gas override (if any) will be used. The next transaction still requires approval.",
    ))
}

pub(super) fn validate_progress(ctx: &DeployContext, key: &str) -> Result<()> {
    let (step, _) = key
        .split_once('/')
        .ok_or_else(|| eyre::eyre!("invalid action key"))?;
    eyre::ensure!(
        next_pending_step(&ctx.state).is_some_and(|(_, next)| next.name == step)
            && ctx
                .state
                .steps
                .iter()
                .any(|s| s.name == step && s.status == StepStatus::Pending),
        "only the next pending step can be retried; completed or dependent actions require reconciliation"
    );
    let proposal = match step {
        "InstantiateChainContracts" => Some("instantiate"),
        "RegisterDeployment" => Some("register"),
        "RegisterItsOnHub" => Some("itsHubRegister"),
        _ => None,
    };
    eyre::ensure!(
        proposal.is_none_or(|key| !ctx.state.proposals.contains_key(key)),
        "a proposal already exists; a rejected/failed proposal cannot use transaction retry"
    );
    Ok(())
}

pub(super) async fn retire(key: &str, gas_limit: Option<u64>) -> Result<()> {
    let session = session::current()?;
    let mut journal = session.journal.lock().await;
    let previous = journal
        .actions
        .remove(key)
        .ok_or_else(|| eyre::eyre!("missing failed action"))?;
    journal
        .attempts
        .entry(key.into())
        .or_default()
        .push(previous);
    if let Some(limit) = gas_limit {
        journal.retry_gas_limits.insert(key.into(), limit);
    }
    storage::atomic_write(&session.path, &serde_json::to_vec_pretty(&*journal)?)
}

pub(super) async fn failed_attempt(key: &str, saved: &Transaction) -> Result<String> {
    let Transaction::Cosmos {
        sender, sequence, ..
    } = saved
    else {
        eyre::bail!("expected Cosmos action");
    };
    for attempt in journal::attempts(key, saved).await? {
        if let Transaction::Cosmos {
            sender: from,
            sequence: seq,
            hash,
            raw,
            ..
        } = attempt
            && from == *sender
            && seq == *sequence
            && matches!(journal::confirmation(&hash).await?, Some(Confirmation::Cosmos { height, code, .. }) if height > 0 && code != 0)
        {
            cosmos_recovery::validate_sequence(&raw, *sequence)?;
            eyre::ensure!(
                hex::encode_upper(sha2::Sha256::digest(&raw)) == hash,
                "failed transaction hash mismatch"
            );
            return Ok(hash);
        }
    }
    eyre::bail!(
        "no confirmed failure for {key}; unknown or successful transactions cannot be retried"
    );
}

pub(super) async fn validate_cosmos_retry(
    key: &str,
    intent: alloy::primitives::B256,
    sequence: u64,
) -> Result<()> {
    let session = session::current()?;
    let journal = session.journal.lock().await;
    if let Some(Transaction::Cosmos {
        intent: expected,
        sequence: previous,
        ..
    }) = journal
        .attempts
        .get(key)
        .and_then(|attempts| attempts.last())
    {
        eyre::ensure!(
            intent == *expected,
            "retried Cosmos execution differs from the retired attempt"
        );
        eyre::ensure!(
            sequence > *previous,
            "retired Cosmos transaction did not consume its sequence; reconcile the account before retrying. No transaction was signed or sent"
        );
    }
    Ok(())
}
