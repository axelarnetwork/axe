use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use alloy::primitives::B256;
use eyre::Result;
use tokio::sync::Mutex;

use super::storage;
use super::types::{Journal, Options, Paused, Plan, Transaction};

pub struct Session {
    pub path: PathBuf,
    pub journal: Mutex<Journal>,
    pub plan: Plan,
    pub rpc: String,
    pub options: Options,
}

tokio::task_local! {
    static SESSION: Arc<Session>;
    static ACTION_SCOPE: (String, String);
}

static HARDENED_REQUIRED: AtomicBool = AtomicBool::new(false);

pub fn require_hardening() {
    HARDENED_REQUIRED.store(true, Ordering::SeqCst);
}

pub fn required() -> bool {
    HARDENED_REQUIRED.load(Ordering::SeqCst)
}

pub fn guard_send() -> Result<()> {
    validate_context(required(), active())
}

pub(super) fn validate_context(required: bool, present: bool) -> Result<()> {
    eyre::ensure!(
        !required || present,
        "hardened session missing; refusing an unjournaled transaction"
    );
    Ok(())
}

pub fn active() -> bool {
    SESSION.try_with(|_| ()).is_ok()
}

pub fn current() -> Result<Arc<Session>> {
    SESSION
        .try_with(Arc::clone)
        .map_err(|_| eyre::eyre!("missing hardened deployment session"))
}

pub fn action_key(label: &str) -> Result<String> {
    ACTION_SCOPE
        .try_with(|(step, _)| format!("{step}/{label}"))
        .map_err(Into::into)
}

pub fn evm_key() -> Result<String> {
    ACTION_SCOPE
        .try_with(|(_, key)| key.clone())
        .map_err(Into::into)
}

pub async fn scope<T>(session: Arc<Session>, future: impl Future<Output = T>) -> T {
    SESSION.scope(session, future).await
}

pub async fn step<T>(name: String, key: String, future: impl Future<Output = T>) -> T {
    ACTION_SCOPE.scope((name, key), future).await
}

pub fn pause(message: impl Into<String>) -> eyre::Report {
    Paused(message.into()).into()
}

impl Session {
    pub async fn load(path: PathBuf, fingerprint: B256, plan: Plan, rpc: String) -> Result<Self> {
        let journal = if path.exists() {
            let journal: Journal = serde_json::from_slice(&tokio::fs::read(&path).await?)?;
            eyre::ensure!(
                journal.version == 2,
                "unsupported deployment journal version; preserve the journal and use its matching binary to recover existing actions"
            );
            eyre::ensure!(
                journal.fingerprint == fingerprint,
                "deployment inputs changed (saved {}, observed {}). Restore the original artifact bytecode, Cosmos storeCodeProposalCodeHash values and public settings before bootstrapping a snapshot for this journal; no fingerprint override is permitted",
                journal.fingerprint,
                fingerprint
            );
            journal
        } else {
            Journal {
                version: 2,
                fingerprint,
                inputs_hash: None,
                evm_confirmations: Default::default(),
                actions: BTreeMap::new(),
                evidence: BTreeMap::new(),
                confirmations: BTreeMap::new(),
                attempts: BTreeMap::new(),
                retry_gas_limits: BTreeMap::new(),
                protocols: BTreeMap::new(),
                protocol_history: Vec::new(),
                initial_signers: None,
                rotations: BTreeMap::new(),
            }
        };
        storage::atomic_write(&path, &serde_json::to_vec_pretty(&journal)?)?;
        Ok(Self {
            path,
            journal: Mutex::new(journal),
            plan,
            rpc,
            options: Options {
                evm_wait_seconds: 1800,
                ..Options::default()
            },
        })
    }

    pub async fn get(&self, key: &str) -> Option<Transaction> {
        self.journal.lock().await.actions.get(key).cloned()
    }

    pub async fn record(&self, key: String, transaction: Transaction) -> Result<()> {
        let mut journal = self.journal.lock().await;
        eyre::ensure!(
            !journal.actions.contains_key(&key),
            "transaction already recorded for {key}"
        );
        journal.actions.insert(key, transaction);
        storage::atomic_write(&self.path, &serde_json::to_vec_pretty(&*journal)?)
    }
}
