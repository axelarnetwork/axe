use alloy::{
    primitives::Address,
    providers::{Provider, ProviderBuilder},
};
use eyre::Result;

use crate::evm::Ownable;
use crate::ui;

/// Print step-by-step `cast send` instructions for setting both sides of the
/// trusted-chains link, so a CLI-driven operator can fix the ITS pairing
/// without leaving the terminal. Looks up the on-chain owner on each side
/// (best-effort) and prints a runnable command per side. Used by the legacy
/// EVM-direct `test its` flow when the destination chain isn't trusted yet.
pub async fn print_untrusted_chain_remediation<P: Provider>(
    axelar_id: &str,
    its_proxy_addr: Address,
    dest_its_addr: Address,
    rpc_url: &str,
    dest_rpc: &str,
    dest_chain: &str,
    provider: &P,
) -> Result<()> {
    ui::error(&format!(
        "\"{dest_chain}\" is not a trusted chain on the ITS at {its_proxy_addr}"
    ));

    let source_owner = Ownable::new(its_proxy_addr, provider)
        .owner()
        .call()
        .await
        .ok();

    let dest_provider = ProviderBuilder::new().connect_http(dest_rpc.parse()?);
    let flow_owner = Ownable::new(dest_its_addr, &dest_provider)
        .owner()
        .call()
        .await
        .ok();

    let mut lines: Vec<String> = vec![
        format!("The ITS on {axelar_id} does not trust \"{dest_chain}\" as a destination chain."),
        String::new(),
        format!("1. On {axelar_id} — set \"{dest_chain}\" as trusted:"),
    ];
    if let Some(owner) = source_owner {
        lines.push(format!("   owner: {owner}"));
    }
    lines.push(format!("   cast send {its_proxy_addr} \\"));
    lines.push("     'setTrustedChain(string)' \\".to_string());
    lines.push(format!("     '{dest_chain}' \\"));
    lines.push(format!("     --rpc-url {rpc_url} \\"));
    lines.push("     --private-key $PRIVATE_KEY".into());
    lines.push(String::new());
    lines.push(format!(
        "2. On {dest_chain} — set \"{axelar_id}\" as trusted:"
    ));
    if let Some(owner) = flow_owner {
        lines.push(format!("   owner: {owner}"));
    }
    lines.push(format!("   cast send {dest_its_addr} \\"));
    lines.push("     'setTrustedChain(string)' \\".to_string());
    lines.push(format!("     '{axelar_id}' \\"));
    lines.push(format!("     --rpc-url {dest_rpc} \\"));
    lines.push("     --private-key $PRIVATE_KEY".into());
    lines.push(String::new());
    lines.push("Both sides must trust each other for cross-chain ITS to work.".into());

    let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
    ui::action_required(&line_refs);
    Ok(())
}
