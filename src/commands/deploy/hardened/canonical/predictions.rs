use std::path::Path;

use alloy::{
    primitives::{Address, b256, keccak256},
    sol_types::SolValue,
};
use eyre::Result;

use super::{CONST_DEPLOYER, CREATE3_DEPLOYER, DEPLOYER, check_address};
use crate::{
    evm::{get_salt_from_key, read_artifact_bytecode},
    types::Network,
    utils::artifact_paths_for_step,
};

pub(super) fn create2(factory: Address, sender: Address, salt: &str, code: &[u8]) -> Address {
    let salt = keccak256((sender, get_salt_from_key(salt)).abi_encode_params());
    factory.create2(salt, keccak256(code))
}

pub(super) fn create3(sender: Address, salt: &str) -> Address {
    // Immutable hash in the canonical Create3Deployer (runtime hash checked before reuse).
    // The standalone CreateDeploy artifact is compiled separately and has different metadata.
    let helper_hash = b256!("f9bf726c56f6eb7a6a041cc888f1adb1231fafec3ca5392bf47fa10cf8df67fd");
    let salt = keccak256((sender, get_salt_from_key(salt)).abi_encode_params());
    CREATE3_DEPLOYER.create2(salt, helper_hash).create(1)
}

pub(super) async fn validate(root: &Path) -> Result<()> {
    for name in ["Create3Deployer", "Operators", "AxelarGasService"] {
        let (implementation, proxy) = artifact_paths_for_step(name, root)
            .ok_or_else(|| eyre::eyre!("missing {name} artifacts"))?;
        let mut code = read_artifact_bytecode(proxy.as_deref().unwrap_or(&implementation)).await?;
        if name == "Operators" {
            code.extend_from_slice(&DEPLOYER.abi_encode());
        }
        check_address(
            Network::Mainnet,
            name,
            create2(CONST_DEPLOYER, DEPLOYER, name, &code),
        )?;
    }
    for (name, salt) in [
        ("InterchainTokenService", "ITS v1.0.0"),
        ("InterchainTokenFactory", "ITS Factory v1.0.0"),
    ] {
        check_address(Network::Mainnet, name, create3(DEPLOYER, salt))?;
    }
    Ok(())
}
