use eyre::Result;

use crate::commands::deploy::DeployContext;
use crate::state::Step;

pub async fn run(ctx: &DeployContext, step: &Step) -> Result<()> {
    let proposal_key = step
        .proposal_key()
        .ok_or_else(|| eyre::eyre!("cosmos-poll step has no proposal_key"))?;
    crate::commands::deploy::hardened::governance::check(ctx, proposal_key).await
}
