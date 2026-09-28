//! `GET /ping` and `GET /info`.

use std::sync::atomic::Ordering;

use axum::Json;
use axum::extract::State;
use davinci_client::api::Info;
use davinci_zkvm_sdk::release;

use super::AppState;

pub async fn ping() -> &'static str {
    "pong"
}

pub async fn info(State(st): State<AppState>) -> Json<Info> {
    let s = &st.node.statics;
    let m = &st.node.metrics;
    Json(Info {
        sequencer_address: s.sequencer_address,
        chain_id: s.chain_id,
        process_registry: s.registry,
        ballot_vk_hash: s.vk_hash,
        batch_program_vk: release::BATCH_PROGRAM_VK,
        results_program_vk: release::RESULTS_PROGRAM_VK,
        observer: s.sequencer_address.is_none(),
        settled_by_self: m.settled_by_self.load(Ordering::Relaxed),
        synced_from_others: m.synced_from_others.load(Ordering::Relaxed),
        lost_races: m.lost_races.load(Ordering::Relaxed),
    })
}
