use eyre::Result;

use super::{
    session, storage,
    types::{Confirmation, Transaction},
};

pub async fn confirmation(hash: &str) -> Result<Option<Confirmation>> {
    if !session::active() {
        return Ok(None);
    }
    Ok(session::current()?
        .journal
        .lock()
        .await
        .confirmations
        .get(hash)
        .cloned())
}

pub async fn confirm(hash: String, result: Confirmation) -> Result<()> {
    if !session::active() {
        return Ok(());
    }
    let session = session::current()?;
    let mut journal = session.journal.lock().await;
    journal.confirmations.insert(hash, result);
    storage::atomic_write(&session.path, &serde_json::to_vec_pretty(&*journal)?)
}

pub async fn replace(key: &str, replacement: Transaction) -> Result<()> {
    let session = session::current()?;
    let mut journal = session.journal.lock().await;
    let previous = journal
        .actions
        .insert(key.into(), replacement)
        .ok_or_else(|| eyre::eyre!("missing action {key}"))?;
    journal
        .attempts
        .entry(key.into())
        .or_default()
        .push(previous);
    storage::atomic_write(&session.path, &serde_json::to_vec_pretty(&*journal)?)
}

pub async fn attempts(key: &str, latest: &Transaction) -> Result<Vec<Transaction>> {
    let mut attempts = if session::active() {
        session::current()?
            .journal
            .lock()
            .await
            .attempts
            .get(key)
            .cloned()
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    attempts.push(latest.clone());
    Ok(attempts)
}
