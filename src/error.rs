pub type AxeResult<T> = std::result::Result<T, AxeError>;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum AxeError {
    #[error("invalid tx_search response: {reason}")]
    InvalidTxSearch { reason: String },

    #[error("tx_search response missing result")]
    TxSearchMissingResult,

    #[error("tx_search result missing tx_result")]
    TxSearchMissingTxResult,

    #[error("wasm-routing event missing '{field}' attribute")]
    MissingWasmRoutingAttribute { field: String },
}
