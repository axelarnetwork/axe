use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Deserialize)]
pub(super) struct ContractQueryFailure {
    pub code: i64,
    pub message: String,
}

#[derive(Deserialize)]
pub struct ProverSet {
    pub id: String,
    pub verifier_set: SignerSet,
}

#[derive(Deserialize)]
pub struct SignerSet {
    pub signers: BTreeMap<String, Signer>,
    pub threshold: Amount,
    pub created_at: u64,
}

#[derive(Deserialize)]
pub struct Signer {
    pub address: String,
    pub pub_key: PublicKey,
    pub weight: Amount,
}

#[derive(Deserialize)]
pub struct PublicKey {
    pub ecdsa: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum Amount {
    Text(String),
    Number(u64),
}

impl Amount {
    pub fn value(&self) -> eyre::Result<u128> {
        match self {
            Self::Text(value) => Ok(value.parse()?),
            Self::Number(value) => Ok(u128::from(*value)),
        }
    }
}
