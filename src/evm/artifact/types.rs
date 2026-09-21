use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RuntimeArtifact {
    pub deployed_bytecode: String,
}
