use serde::Deserialize;

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ProverConfig {
    pub address: Option<String>,
    pub admin_address: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct RawResponse {
    pub data: String,
}
