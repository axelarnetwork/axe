use super::{session, storage, types::ProtocolIdentity};
use crate::ui;
use eyre::Result;
use std::collections::BTreeMap;

pub async fn approve(observed: BTreeMap<String, ProtocolIdentity>) -> Result<()> {
    let session = session::current()?;
    let previous = session.journal.lock().await.protocols.clone();
    if previous == observed {
        return Ok(());
    }
    if !previous.is_empty() {
        validate_change(&previous, &observed)?;
        for (name, current) in &observed {
            if previous.get(name) != Some(current) {
                ui::kv("protocol changed", name);
                ui::info(&format!(
                    "Previous: {}\nCurrent: {}",
                    serde_json::to_string(&previous[name])?,
                    serde_json::to_string(current)?
                ));
            }
        }
        ui::info(
            "Protocol wiring and authority checks passed. Review the governance migration and these code hashes before continuing.",
        );
        if !ui::confirm("Accept these protocol code changes for this deployment?").await {
            return Err(session::pause("protocol changes not approved"));
        }
    }
    let mut journal = session.journal.lock().await;
    if !previous.is_empty() {
        journal.protocol_history.push(previous);
    }
    journal.protocols = observed;
    storage::atomic_write(&session.path, &serde_json::to_vec_pretty(&*journal)?)
}

pub(super) fn validate_change(
    old: &BTreeMap<String, ProtocolIdentity>,
    new: &BTreeMap<String, ProtocolIdentity>,
) -> Result<()> {
    eyre::ensure!(old.len() == new.len(), "protocol contract set changed");
    for (name, a) in old {
        let b = new
            .get(name)
            .ok_or_else(|| eyre::eyre!("protocol contract missing: {name}"))?;
        eyre::ensure!(
            a.code_id != b.code_id || a.checksum == b.checksum,
            "{name}: immutable code checksum changed without a new code ID"
        );
        eyre::ensure!(
            a.address == b.address && a.creator == b.creator && a.admin == b.admin,
            "{name}: protocol address or authority changed; code-upgrade approval cannot override it"
        );
    }
    Ok(())
}
