use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputSnapshot {
    pub version: u32,
    pub artifacts: BTreeMap<String, Value>,
    pub cosmos_codes: BTreeMap<String, String>,
    pub fingerprint_inputs: Option<Value>,
}

pub struct PreparedInputs {
    pub snapshot: InputSnapshot,
    pub directory: PathBuf,
}
