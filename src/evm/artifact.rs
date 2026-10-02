mod types;

use alloy::primitives::{B256, keccak256};
use eyre::{Result, WrapErr, ensure};

use types::RuntimeArtifact;

pub async fn read_artifact_runtime_hash(artifact_path: &str) -> Result<B256> {
    let content = tokio::fs::read_to_string(artifact_path)
        .await
        .wrap_err_with(|| format!("cannot read artifact {artifact_path}"))?;
    runtime_hash(&content)
        .wrap_err_with(|| format!("invalid runtime bytecode in artifact {artifact_path}"))
}

fn runtime_hash(content: &str) -> Result<B256> {
    let artifact: RuntimeArtifact = serde_json::from_str(content)?;
    let bytecode = artifact.deployed_bytecode;
    let runtime = hex::decode(bytecode.strip_prefix("0x").unwrap_or(&bytecode))?;
    ensure!(!runtime.is_empty(), "deployedBytecode is empty");
    Ok(keccak256(runtime))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::utils::artifact_paths_for_step;

    #[tokio::test]
    async fn reproduces_robinhood_deployer_hashes_from_selected_artifacts() {
        let root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/deployment-artifacts");
        for (step, expected) in [
            (
                "ConstAddressDeployer",
                "0x8fda47a596dfba923270da84e0c32a2d0312f1c03389f83e16f2b5a35ed37fbe",
            ),
            (
                "Create3Deployer",
                "0x73fc31262c4bad113c79439fd231281201c7c7d45b50328bd86bccf37684bf92",
            ),
            (
                "Operators",
                "0xc561dc32ef670c929db9d7fbf6b5f6c074a62a30602481ba3b88912ca6d79feb",
            ),
        ] {
            let (artifact, _) = artifact_paths_for_step(step, &root).unwrap();
            assert_eq!(
                read_artifact_runtime_hash(&artifact)
                    .await
                    .unwrap()
                    .to_string(),
                expected,
                "{step}"
            );
        }
    }

    #[test]
    fn hashes_runtime_instead_of_creation_bytecode() {
        let artifact = r#"{"bytecode":"0x6000","deployedBytecode":"0x6001"}"#;
        assert_eq!(runtime_hash(artifact).unwrap(), keccak256([0x60, 0x01]));
        assert_ne!(runtime_hash(artifact).unwrap(), keccak256([0x60, 0x00]));
    }

    #[test]
    fn rejects_missing_empty_or_invalid_runtime_bytecode() {
        for artifact in [
            r#"{"bytecode":"0x6000"}"#,
            r#"{"deployedBytecode":"0x"}"#,
            r#"{"deployedBytecode":""}"#,
            r#"{"deployedBytecode":"0xnothex"}"#,
        ] {
            assert!(runtime_hash(artifact).is_err(), "{artifact}");
        }
    }
}
