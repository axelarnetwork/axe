use eyre::Result;

use super::types::{ChainCodeIds, InstantiateAccess, InstantiatePermission, StoredCode};

#[cfg(test)]
mod tests;

fn allows_coordinator(permission: &InstantiatePermission, coordinator: &str) -> bool {
    match permission.permission {
        InstantiateAccess::Everybody => true,
        InstantiateAccess::AnyOfAddresses => permission
            .addresses
            .iter()
            .any(|address| address == coordinator),
        InstantiateAccess::Nobody | InstantiateAccess::Unknown => false,
    }
}

fn validate_permissions(
    coordinator: &str,
    permissions: &[(&str, u64, InstantiatePermission)],
) -> Result<()> {
    let denied: Vec<_> = permissions
        .iter()
        .filter(|(_, _, permission)| !allows_coordinator(permission, coordinator))
        .map(|(name, code, permission)| {
            format!(
                "{name} code {code}: {:?}, allowed addresses: [{}]",
                permission.permission,
                permission.addresses.join(", ")
            )
        })
        .collect();
    eyre::ensure!(
        denied.is_empty(),
        "Coordinator {coordinator} is not authorized to instantiate:\n  - {}\nGrant this Coordinator address instantiate permission on these code IDs through governance, preserving existing allowed addresses. Changing MNEMONIC or EVM private keys will not fix this",
        denied.join("\n  - ")
    );
    Ok(())
}

pub(super) async fn check(lcd: &str, coordinator: &str, codes: &ChainCodeIds) -> Result<()> {
    let mut permissions = Vec::new();
    for (name, code) in [
        ("Gateway", codes.gateway),
        ("VotingVerifier", codes.verifier),
        ("MultisigProver", codes.prover),
    ] {
        let url = format!("{}/cosmwasm/wasm/v1/code/{code}", lcd.trim_end_matches('/'));
        let stored: StoredCode = crate::http::client()
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        permissions.push((name, code, stored.code_info.instantiate_permission));
    }
    validate_permissions(coordinator, &permissions)
}
