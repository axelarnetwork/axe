use std::collections::BTreeSet;

use alloy::primitives::{B256, U256};
use comfy_table::{ContentArrangement, Table};
use eyre::Result;
use serde_json::json;

use super::{
    session, storage,
    types::{ActiveVerifier, InitialSigners, Service},
    verification,
    verifier_types::{ContractQueryFailure, ProverSet},
    verifiers,
};
use crate::{
    commands::verifiers::lookup_name,
    cosmos::{
        CosmwasmQueryError, lcd_cosmwasm_smart_query, lcd_cosmwasm_smart_query_typed,
        read_axelar_config,
    },
    state::State,
    types::Network,
    ui,
};

pub async fn query(state: &State) -> Result<Option<InitialSigners>> {
    let (lcd, _, _, _) = read_axelar_config(&state.target_json).await?;
    let prover = verifiers::contract(state, "MultisigProver", true).await?;
    let response = lcd_cosmwasm_smart_query(&lcd, &prover, &json!("current_verifier_set")).await?;
    let Some(set) = serde_json::from_value::<Option<ProverSet>>(response)? else {
        return Ok(None);
    };
    let registry = verifiers::contract(state, "ServiceRegistry", false).await?;
    let service = verifiers::service(state, &lcd, &registry).await?;
    let threshold = state
        .hardened_plan
        .as_ref()
        .ok_or_else(|| eyre::eyre!("missing deployment plan"))?
        .signing_threshold;
    Ok(Some(validate(set, &service, threshold)?))
}

pub(super) fn validate_limits(service: &Service) -> Result<()> {
    eyre::ensure!(
        service.min_num_verifiers > 0,
        "service minimum must be positive"
    );
    eyre::ensure!(
        service
            .max_num_verifiers
            .is_none_or(|max| max >= service.min_num_verifiers),
        "invalid service verifier limits"
    );
    Ok(())
}

fn validate_count(count: usize, service: &Service) -> Result<()> {
    validate_limits(service)?;
    eyre::ensure!(
        count as u64 >= service.min_num_verifiers
            && service
                .max_num_verifiers
                .is_none_or(|max| count as u64 <= max),
        "verifier count {count} is outside service limits (minimum {}, maximum {:?})",
        service.min_num_verifiers,
        service.max_num_verifiers
    );
    Ok(())
}

pub(super) fn validate(
    set: ProverSet,
    service: &Service,
    fraction: [u64; 2],
) -> Result<InitialSigners> {
    validate_count(set.verifier_set.signers.len(), service)?;
    eyre::ensure!(!set.id.is_empty(), "missing prover verifier set ID");
    let mut members = Vec::new();
    let mut total = 0u128;
    for (identity, signer) in set.verifier_set.signers {
        let account: cosmrs::AccountId = identity.parse()?;
        eyre::ensure!(
            account.prefix() == "axelar" && signer.address == identity,
            "prover signer identity mismatch"
        );
        let weight = signer.weight.value()?;
        eyre::ensure!(weight > 0, "zero signer weight");
        total = total
            .checked_add(weight)
            .ok_or_else(|| eyre::eyre!("signer weight overflow"))?;
        let key = signer.pub_key.ecdsa.trim_start_matches("0x");
        let key = crate::evm::pubkey_to_address(&hex::decode(key)?)?;
        members.push((key, identity, weight));
    }
    members.sort_by_key(|(key, _, _)| *key);
    eyre::ensure!(
        members.windows(2).all(|pair| pair[0].0 < pair[1].0),
        "duplicate signer keys"
    );
    eyre::ensure!(
        fraction[0] > 0 && fraction[0] <= fraction[1],
        "invalid signing threshold"
    );
    let expected = total
        .checked_mul(u128::from(fraction[0]))
        .ok_or_else(|| eyre::eyre!("threshold overflow"))?
        .div_ceil(u128::from(fraction[1]));
    let threshold = set.verifier_set.threshold.value()?;
    eyre::ensure!(
        threshold > 0 && threshold == expected,
        "prover signing threshold differs from deployment plan"
    );
    let identities = members
        .iter()
        .map(|(_, identity, _)| identity.clone())
        .collect();
    let signers = members
        .into_iter()
        .map(|(key, _, weight)| (key, weight))
        .collect();
    let nonce = B256::from(U256::from(set.verifier_set.created_at).to_be_bytes::<32>());
    Ok(verification::snapshot(
        identities, signers, threshold, nonce, set.id,
    ))
}

pub(super) fn name(network: Network, address: &str) -> &'static str {
    lookup_name(network, address).unwrap_or("Unknown (not in axe's address book)")
}

pub(super) fn insufficient_verifiers(error: &CosmwasmQueryError) -> bool {
    let CosmwasmQueryError::Http { status, body, .. } = error else {
        return false;
    };
    *status == reqwest::StatusCode::INTERNAL_SERVER_ERROR
        && serde_json::from_str::<ContractQueryFailure>(body).is_ok_and(|error| {
            error.code == 2 && error.message == "not enough verifiers: query wasm contract failed"
        })
}

pub async fn show_candidates(state: &State) -> Result<()> {
    let (lcd, _, _, _) = read_axelar_config(&state.target_json).await?;
    let registry = verifiers::contract(state, "ServiceRegistry", false).await?;
    let service = verifiers::service(state, &lcd, &registry).await?;
    let response = lcd_cosmwasm_smart_query_typed(&lcd, &registry,
        &json!({"active_verifiers":{"service_name":state.env.verifier_service_name(),"chain_name":state.axelar_id}})).await;
    let response = match response {
        Ok(response) => response,
        Err(error) if insufficient_verifiers(&error) => {
            ui::warn(&format!(
                "{} is not ready for activation: ServiceRegistry requires at least {} eligible verifiers (authorized, sufficiently bonded, and registered for this chain).",
                state.axelar_id, service.min_num_verifiers
            ));
            ui::info("The registry reports fewer than the minimum; this response does not include an exact count. The prover set has not been initialized.");
            super::handoff::verifiers(state).await?;
            return Err(session::pause("Waiting for verifier registrations. Complete the rollout and registration steps above, then resume with --activate. No verifier initialization transaction was submitted."));
        }
        Err(error) => return Err(eyre::Report::new(error).wrap_err(
            "Could not read eligible verifiers. Resolve the query error below, then resume; no verifier initialization transaction was submitted."
        )),
    };
    let active: Vec<ActiveVerifier> = serde_json::from_value(response)?;
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(["Name in axe", "Axelar address"]);
    let mut unique = BTreeSet::new();
    for verifier in &active {
        let address = &verifier.verifier_info.address;
        let account: cosmrs::AccountId = address.parse()?;
        eyre::ensure!(
            account.prefix() == "axelar" && unique.insert(address),
            "invalid or duplicate eligible identity"
        );
        table.add_row([name(state.env, address), address]);
    }
    ui::section("Eligible registered verifiers");
    println!("{table}");
    ui::kv(
        "eligible / service minimum",
        &format!("{} / {}", active.len(), service.min_num_verifiers),
    );
    validate_count(active.len(), &service)?;
    ui::info(
        "Names are local labels, not authorization. The registry determines eligibility. Prover initialization is simulated before broadcast; its actual keyed signer set is reviewed afterwards.",
    );
    Ok(())
}

pub(super) fn same_set(a: &InitialSigners, b: &InitialSigners) -> bool {
    a.hash == b.hash
        && a.set_id == b.set_id
        && a.identities.iter().collect::<BTreeSet<_>>()
            == b.identities.iter().collect::<BTreeSet<_>>()
}

pub async fn approve(state: &State, initial: InitialSigners) -> Result<()> {
    let session = session::current()?;
    eyre::ensure!(
        session.get("AxelarGateway/gateway proxy").await.is_none(),
        "gateway proxy already journaled; its initial signer set cannot be replaced"
    );
    ui::section("Review the actual initial prover set");
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(["Name in axe", "Axelar address", "EVM signer", "Weight"]);
    for (identity, (key, weight)) in initial.identities.iter().zip(&initial.signers) {
        table.add_row([
            name(state.env, identity).to_owned(),
            identity.clone(),
            key.to_string(),
            weight.to_string(),
        ]);
    }
    println!("{table}");
    ui::kv("verifier count", &initial.signers.len().to_string());
    let known = initial
        .identities
        .iter()
        .filter(|address| lookup_name(state.env, address).is_some())
        .count();
    ui::kv(
        "known / unknown names",
        &format!("{known} / {}", initial.identities.len() - known),
    );
    ui::kv("required signing weight", &initial.threshold.to_string());
    ui::kv(
        "total signing weight",
        &initial
            .signers
            .iter()
            .map(|(_, weight)| weight)
            .sum::<u128>()
            .to_string(),
    );
    ui::kv("prover set ID", &initial.set_id);
    ui::kv("gateway signer hash", &initial.hash.to_string());
    ui::info(
        "Known names are informational. Unknown addresses are not rejected automatically. Approving saves this exact set for the gateway constructor; it does not authorize verifiers on-chain.",
    );
    let approved = ui::confirm("Approve this initial verifier set for gateway deployment?").await;
    save_decision(initial, approved).await
}

pub(super) async fn save_decision(initial: InitialSigners, approved: bool) -> Result<()> {
    if !approved {
        return Err(session::pause(
            "Initial verifier set declined. The prover initialization remains recorded; no gateway proxy will be sent. Review the verifier identities with their operators, then run the continue command to review the set again.",
        ));
    }
    let session = session::current()?;
    let mut journal = session.journal.lock().await;
    eyre::ensure!(
        !journal.actions.contains_key("AxelarGateway/gateway proxy"),
        "gateway proxy already journaled; its initial signer set cannot be replaced"
    );
    journal.initial_signers = Some(initial);
    storage::atomic_write(&session.path, &serde_json::to_vec_pretty(&*journal)?)
}
