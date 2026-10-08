use alloy::primitives::{B256, U256};
use alloy::sol_types::SolValue;

use super::gateway_implementation_code;
use crate::types::Network;

#[tokio::test]
async fn gateway_constructor_encodes_live_network_rotation_delays() {
    let artifact = std::env::temp_dir().join(format!(
        "axe-gateway-rotation-{}.json",
        rand::random::<u64>()
    ));
    tokio::fs::write(&artifact, r#"{"bytecode":"0x6000"}"#)
        .await
        .unwrap();
    let domain = B256::repeat_byte(0x42);
    for (network, delay) in [
        (Network::Mainnet, 86_400u64),
        (Network::Testnet, 3_600),
        (Network::Stagenet, 300),
        (Network::DevnetAmplifier, 0),
    ] {
        let code = gateway_implementation_code(artifact.to_str().unwrap(), domain, network)
            .await
            .unwrap();
        assert_eq!(&code[..2], &[0x60, 0x00]);
        let decoded = <(U256, B256, U256)>::abi_decode(&code[2..]).unwrap();
        assert_eq!(decoded, (U256::from(15), domain, U256::from(delay)));
    }
    tokio::fs::remove_file(artifact).await.unwrap();
}
