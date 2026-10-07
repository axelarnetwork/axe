use alloy::primitives::{B256, U256, keccak256};
use alloy::rpc::types::TransactionRequest;
use eyre::Result;

/// Versioned, explicit encoding independent of Alloy's JSON representation.
pub fn hash(request: &TransactionRequest) -> Result<B256> {
    eyre::ensure!(
        request
            .access_list
            .as_ref()
            .is_none_or(|list| list.0.is_empty())
            && request.authorization_list.is_none()
            && request.blob_versioned_hashes.is_none(),
        "unsupported deployment transaction extensions"
    );
    let mut bytes = b"axe-deploy-intent-v2".to_vec();
    bytes.extend_from_slice(
        &request
            .chain_id
            .ok_or_else(|| eyre::eyre!("missing chain ID"))?
            .to_be_bytes(),
    );
    bytes.extend_from_slice(
        request
            .from
            .ok_or_else(|| eyre::eyre!("missing signer"))?
            .as_slice(),
    );
    match request.to.and_then(|to| to.to().copied()) {
        Some(address) => {
            bytes.push(1);
            bytes.extend_from_slice(address.as_slice());
        }
        None => bytes.push(0),
    }
    bytes.extend_from_slice(&request.value.unwrap_or(U256::ZERO).to_be_bytes::<32>());
    for value in [request.nonce, request.gas] {
        bytes.push(u8::from(value.is_some()));
        bytes.extend_from_slice(&value.unwrap_or_default().to_be_bytes());
    }
    bytes.extend_from_slice(request.input.input().map_or(&[], |data| data.as_ref()));
    Ok(keccak256(bytes))
}
