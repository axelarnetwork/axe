//! Solana integration. Wallet preflight, RPC plumbing, ITS encoding helpers,
//! gateway send + manual approval flow, and ITS instruction builders.
//!
//! Submodules are layered:
//!
//! - [`balance`]: per-flow lamport thresholds + `check_solana_balance`.
//! - [`rpc`]: confirmed-commitment client + the retry loop that hides testnet
//!   RPC lag past `send_and_confirm`.
//! - [`encoding`]: pure ITS PDA / ATA derivation, the interchain-token-id
//!   digest, and keypair loading.
//! - [`gateway`]: source-side `call_contract`, log/event extraction for
//!   message-id and payload, and the destination-side manual approval flow.
//! - [`its`]: ITS deploy/transfer instruction builders.

mod balance;
mod encoding;
mod gateway;
mod its;
mod rpc;

pub use balance::{
    MIN_SOL_ITS_LAMPORTS, MIN_SOL_RELAY_LAMPORTS, MIN_SOL_SEND_LAMPORTS, check_solana_balance,
};
pub use encoding::{
    find_interchain_token_pda, find_its_root_pda, interchain_token_id, load_keypair,
};
pub use gateway::{
    approve_messages_on_gateway, decode_execute_data, execute_on_memo,
    extract_gateway_call_contract_payload, extract_its_message_id, send_call_contract,
    solana_call_contract_index,
};
pub use its::{
    send_its_deploy_interchain_token, send_its_deploy_remote_interchain_token,
    send_its_interchain_transfer,
};
pub use rpc::{rpc_client, wait_for_signature_finalized};

#[cfg(test)]
mod tests {
    use solana_sdk::pubkey::Pubkey;

    #[test]
    fn derive_testnet_gateway_pdas() {
        // Testnet Solana gateway
        let gateway_id: Pubkey = "gtwJ8LWDRWZpbvCqp8sDeTgy3GSyuoEsiaKC8wSXJqq"
            .parse()
            .unwrap();

        // GatewayConfig PDA
        let (config_pda, _) = Pubkey::find_program_address(&[b"gateway"], &gateway_id);
        println!("GatewayConfig PDA: {config_pda}");
        assert_eq!(
            config_pda.to_string(),
            "8mnEaWDXqbpDwyiGLR1T8DTc8AHuk2Fs6Pf4fRDv97WY"
        );

        // VerifierSetTracker PDA for the on-chain verifier set
        let onchain_hash =
            hex::decode("7b8163c3123a65f351c1d5b1e94c44841e731ea57b51f55479207380cab933c5")
                .unwrap();
        let (tracker_pda, _) =
            Pubkey::find_program_address(&[b"ver-set-tracker", &onchain_hash], &gateway_id);
        println!("VerifierSetTracker PDA (on-chain):  {tracker_pda}");
        assert_eq!(
            tracker_pda.to_string(),
            "F1PVLJQSGxBr28QWsRJTaTJiua7yKZQ5r97KG154uZum"
        );

        // VerifierSetTracker PDA for the MultisigProver's current set
        let prover_hash =
            hex::decode("046c15e70bf840b19ef2e727bbfe6fae18155077342b2aa41d766a2f6db32cb1")
                .unwrap();
        let (tracker_pda2, _) =
            Pubkey::find_program_address(&[b"ver-set-tracker", &prover_hash], &gateway_id);
        println!("VerifierSetTracker PDA (prover):    {tracker_pda2}");

        // These should be DIFFERENT — confirming the mismatch
        assert_ne!(tracker_pda, tracker_pda2);
        println!("\nVerifier set mismatch confirmed!");
        println!("Gateway knows:      7b8163c3...");
        println!("MultisigProver uses: 046c15e7...");
        println!("rotate_signers needed on the Solana gateway");
    }
}

#[cfg(test)]
#[test]
fn derive_devnet_gateway_pdas() {
    use solana_sdk::pubkey::Pubkey;

    let gateway_id: Pubkey = "gtwT4uGVTYSPnTGv6rSpMheyFyczUicxVWKqdtxNGw9"
        .parse()
        .unwrap();

    let (config_pda, _) = Pubkey::find_program_address(&[b"gateway"], &gateway_id);
    println!("=== DEVNET-AMPLIFIER ===");
    println!("GatewayConfig PDA: {config_pda}");

    // MultisigProver verifier set: caa238976160fcea5d5e5f4f3ea2ce0bed9106847e2d6d939de746c890c1faed
    let prover_hash =
        hex::decode("caa238976160fcea5d5e5f4f3ea2ce0bed9106847e2d6d939de746c890c1faed").unwrap();
    let (tracker_pda, _) =
        Pubkey::find_program_address(&[b"ver-set-tracker", &prover_hash], &gateway_id);
    println!("VerifierSetTracker PDA (prover set): {tracker_pda}");
    println!("Check on-chain: solana account {tracker_pda} --url https://api.devnet.solana.com");
}

#[cfg(test)]
#[test]
fn derive_stagenet_gateway_pdas() {
    use solana_sdk::pubkey::Pubkey;

    let gateway_id: Pubkey = "gtwYHfHHipAoj8Hfp3cGr3vhZ8f3UtptGCQLqjBkaSZ"
        .parse()
        .unwrap();

    let (config_pda, _) = Pubkey::find_program_address(&[b"gateway"], &gateway_id);
    println!("=== STAGENET ===");
    println!("GatewayConfig PDA: {config_pda}");

    // MultisigProver verifier set: 315ad3ca3e873b65dbc5dd4a446a62018ea368b5d9f29232fa090875fdaa50b8
    let prover_hash =
        hex::decode("315ad3ca3e873b65dbc5dd4a446a62018ea368b5d9f29232fa090875fdaa50b8").unwrap();
    let (tracker_pda, _) =
        Pubkey::find_program_address(&[b"ver-set-tracker", &prover_hash], &gateway_id);
    println!("VerifierSetTracker PDA (prover set): {tracker_pda}");
    println!("Check on-chain: solana account {tracker_pda} --url https://api.testnet.solana.com");
}
