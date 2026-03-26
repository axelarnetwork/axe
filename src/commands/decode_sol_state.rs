use eyre::{Result, bail};
use owo_colors::OwoColorize;
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::account::Account;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

// ---------------------------------------------------------------------------
// All known gateway and ITS program IDs across networks
// ---------------------------------------------------------------------------

const GATEWAY_IDS: &[(&str, &str)] = &[
    ("gtwT4uGVTYSPnTGv6rSpMheyFyczUicxVWKqdtxNGw9", "devnet"),
    ("gtwYHfHHipAoj8Hfp3cGr3vhZ8f3UtptGCQLqjBkaSZ", "stagenet"),
    ("gtwJ8LWDRWZpbvCqp8sDeTgy3GSyuoEsiaKC8wSXJqq", "testnet"),
];

const ITS_IDS: &[(&str, &str)] = &[
    ("itsm3zZhp2oGgEfq7XBu9ojRCYZJnhzecbAEPCrvx2B", "devnet"),
    ("itsYxmqAxNKUL5zaj3fD1K1whuVhqpxKVoiLGie1reF", "devnet-old"),
    ("itsm3zZhp2oGgEfq7XBu9ojRCYZJnhzecbAEPCrvx2B", "stagenet"),
    ("itsJo4kNJ3mdh3requwbtTTt7vyYTudp1pxhn2KiHMc", "testnet"),
];

const SOLANA_RPCS: &[(&str, &str)] = &[
    ("devnet", "https://api.devnet.solana.com"),
    ("testnet", "https://api.testnet.solana.com"),
    ("mainnet", "https://api.mainnet-beta.solana.com"),
];

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

pub async fn run(rpc_override: Option<&str>) -> Result<()> {
    let rpcs: Vec<(&str, &str)> = if let Some(rpc) = rpc_override {
        vec![("custom", rpc)]
    } else {
        SOLANA_RPCS.to_vec()
    };

    for (network, rpc_url) in &rpcs {
        let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

        // Find gateways on this network
        for (gw_addr, gw_network) in GATEWAY_IDS {
            let gw_pubkey = Pubkey::from_str(gw_addr).unwrap();
            let gateway_pda = Pubkey::find_program_address(&[b"gateway"], &gw_pubkey).0;

            if let Ok(account) = rpc.get_account(&gateway_pda) {
                println!(
                    "\n{}",
                    format!("━━ Gateway ({gw_network}) on Solana {network} ━━").bold()
                );
                println!("  {} {}", "program:".dimmed(), gw_addr);
                println!("  {} {}", "config PDA:".dimmed(), gateway_pda);
                dump_gateway_config(&account, &rpc, &gw_pubkey)?;
            }
        }

        // Find ITS on this network
        for (its_addr, its_network) in ITS_IDS {
            let its_pubkey = Pubkey::from_str(its_addr).unwrap();
            let its_pda =
                Pubkey::find_program_address(&[b"interchain-token-service"], &its_pubkey).0;

            if let Ok(account) = rpc.get_account(&its_pda) {
                println!(
                    "\n{}",
                    format!("━━ ITS ({its_network}) on Solana {network} ━━").bold()
                );
                println!("  {} {}", "program:".dimmed(), its_addr);
                println!("  {} {}", "root PDA:".dimmed(), its_pda);
                dump_its_config(&account)?;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Gateway config decoding (zero_copy account)
// ---------------------------------------------------------------------------

fn dump_gateway_config(account: &Account, rpc: &RpcClient, gateway_id: &Pubkey) -> Result<()> {
    let data = &account.data;

    // zero_copy accounts have 8-byte Anchor discriminator
    if data.len() < 8 + 32 + 32 + 8 + 8 + 32 + 32 + 1 {
        bail!("gateway config account too small: {} bytes", data.len());
    }

    let d = &data[8..]; // skip discriminator

    // GatewayConfig layout (zero_copy):
    //   current_epoch: U256 ([u64;4] = 32 bytes)
    //   previous_verifier_set_retention: U256 (32 bytes)
    //   minimum_rotation_delay: u64 (8 bytes)
    //   last_rotation_timestamp: u64 (8 bytes)
    //   operator: Pubkey (32 bytes)
    //   domain_separator: [u8;32] (32 bytes)
    //   bump: u8
    //   _padding: [u8;7]

    let epoch = u64::from_le_bytes(d[0..8].try_into()?);
    let retention = u64::from_le_bytes(d[32..40].try_into()?);
    let min_rotation_delay = u64::from_le_bytes(d[64..72].try_into()?);
    let last_rotation_ts = u64::from_le_bytes(d[72..80].try_into()?);
    let operator = Pubkey::try_from(&d[80..112])?;
    let domain_separator = hex::encode(&d[112..144]);
    let bump = d[144];

    println!("  {} {}", "epoch:".dimmed(), epoch);
    println!("  {} {}", "retention:".dimmed(), retention);
    println!(
        "  {} {}s",
        "min_rotation_delay:".dimmed(),
        min_rotation_delay
    );
    if last_rotation_ts > 0 {
        println!("  {} {}", "last_rotation:".dimmed(), last_rotation_ts);
    }
    println!("  {} {}", "operator:".dimmed(), operator);
    println!("  {} {}", "domain_separator:".dimmed(), domain_separator);
    println!("  {} {}", "bump:".dimmed(), bump);

    // Enumerate verifier set trackers
    println!("\n  {}", "Verifier Sets:".bold());
    for e in 1..=epoch {
        // We don't know the hash for each epoch, but we can try to find them
        // by scanning. Instead, let's just show the current epoch's tracker.
        // The tracker PDA needs the hash, which we don't have from the config alone.
        // We'll scan recent ones by trying known tracker accounts.
        let _ = e; // epochs are tracked by hash, not by number
    }

    // Try to find verifier set trackers by getProgramAccounts
    dump_verifier_set_trackers(rpc, gateway_id, epoch)?;

    Ok(())
}

fn dump_verifier_set_trackers(
    rpc: &RpcClient,
    gateway_id: &Pubkey,
    current_epoch: u64,
) -> Result<()> {
    use solana_client::rpc_filter::{Memcmp, RpcFilterType};
    use solana_sdk::account::ReadableAccount;

    // sha256("account:VerifierSetTracker")[0:8]
    let disc = vec![0x29, 0x08, 0xa3, 0x9d, 0xe5, 0xe9, 0x14, 0xb5];

    let filters = vec![RpcFilterType::Memcmp(Memcmp::new_raw_bytes(0, disc))];

    let accounts = match rpc.get_program_accounts_with_config(
        gateway_id,
        solana_client::rpc_config::RpcProgramAccountsConfig {
            filters: Some(filters),
            ..Default::default()
        },
    ) {
        Ok(accs) => accs,
        Err(_) => {
            println!("    (could not enumerate verifier set trackers)");
            return Ok(());
        }
    };

    if accounts.is_empty() {
        println!("    (no verifier set trackers found)");
        return Ok(());
    }

    for (pubkey, account) in &accounts {
        let data = account.data();
        if data.len() < 80 {
            continue;
        }
        let d = &data[8..]; // skip discriminator
        // bump: u8, _padding: [u8;7], epoch: U256, verifier_set_hash: [u8;32]
        let epoch = u64::from_le_bytes(d[8..16].try_into().unwrap_or_default());
        let hash = hex::encode(&d[40..72]);
        let is_current = epoch == current_epoch;
        let marker = if is_current {
            " ← current".green().to_string()
        } else {
            String::new()
        };
        println!(
            "    {} {} {} {}{}",
            "epoch:".dimmed(),
            epoch,
            "hash:".dimmed(),
            &hash[..16],
            marker,
        );
        println!("      {} {}", "PDA:".dimmed(), pubkey);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// ITS config decoding (Anchor account, borsh-serialized)
// ---------------------------------------------------------------------------

fn dump_its_config(account: &Account) -> Result<()> {
    let data = &account.data;
    if data.len() < 8 {
        bail!("ITS account too small: {} bytes", data.len());
    }

    // InterchainTokenService is a regular Anchor #[account], borsh-serialized
    // Fields: its_hub_address: String, chain_name: String, paused: bool,
    //         trusted_chains: Vec<String>, bump: u8
    let d = &data[8..]; // skip discriminator

    let (hub_address, rest) = decode_borsh_string(d)?;
    let (chain_name, rest) = decode_borsh_string(rest)?;

    if rest.is_empty() {
        bail!("ITS data truncated");
    }
    let paused = rest[0] != 0;
    let rest = &rest[1..];

    // trusted_chains: Vec<String> — borsh Vec is 4-byte len + repeated strings
    let mut trusted_chains = Vec::new();
    if rest.len() >= 4 {
        let count = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        let mut pos = &rest[4..];
        for _ in 0..count {
            if let Ok((chain, remaining)) = decode_borsh_string(pos) {
                trusted_chains.push(chain);
                pos = remaining;
            } else {
                break;
            }
        }
    }

    println!("  {} \"{}\"", "hub_address:".dimmed(), hub_address);
    println!("  {} \"{}\"", "chain_name:".dimmed(), chain_name);
    println!(
        "  {} {}",
        "paused:".dimmed(),
        if paused {
            "true".red().to_string()
        } else {
            "false".green().to_string()
        }
    );
    println!(
        "  {} {} chains",
        "trusted_chains:".dimmed(),
        trusted_chains.len()
    );
    for chain in &trusted_chains {
        println!("    - {chain}");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn decode_borsh_string(data: &[u8]) -> Result<(String, &[u8])> {
    if data.len() < 4 {
        bail!("not enough data for string length");
    }
    let len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if data.len() < 4 + len {
        bail!("not enough data for string content");
    }
    let s = String::from_utf8_lossy(&data[4..4 + len]).to_string();
    Ok((s, &data[4 + len..]))
}
