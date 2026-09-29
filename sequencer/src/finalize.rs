//! The finalizer: proves the single-key results circuit and settles the
//! results on-chain. Runs only on the node holding the election secret;
//! everyone else just watches for `ResultsSet`. DKG-mode processes have no
//! such node: any signing node hands the accumulator to the committee and
//! publishes its plaintexts (`run_finalize_dkg`).

use std::sync::Arc;
use std::time::Duration;

use davinci_zkvm_sdk::client::PlonkSnark;
use davinci_zkvm_sdk::limits::NUM_FIELDS;
use davinci_zkvm_sdk::publics::{ResultsPublics, results_fail_bits};
use davinci_zkvm_sdk::release;
use davinci_zkvm_sdk::types::ResultsRequest;
use rand::Rng;
use tokio::sync::mpsc;
use tracing::info;

use crate::actor::{Chain, FinalizeFail, Msg, Prover};
use crate::web3::{ProcessStatus, Web3Error};

/// One finalize attempt: prove the results circuit, check the proof, submit.
/// With `hold` (eager results, window still open) the checked snark goes back
/// to the actor instead, which submits it once the grace ends. The actor owns
/// retries: a transient failure re-arms after a cooldown, a failed check or a
/// revert latches. Reports through the actor's mailbox.
#[allow(clippy::too_many_arguments)] // a one-shot task, not an API
pub(crate) async fn run_finalize(
    prover: Arc<dyn Prover>,
    chain: Arc<dyn Chain>,
    pid31: [u8; 31],
    request: ResultsRequest,
    expected_root: [u8; 32],
    tally: [u64; NUM_FIELDS],
    max_total: u64,
    hold: bool,
    generation: u64,
    tx: mpsc::Sender<Msg>,
) {
    let attempt = async {
        if !hold {
            window_still_closed(&*chain, &pid31, max_total).await?;
        }
        let snark = prove_checked(&*prover, &request, &expected_root, &tally).await?;
        if hold {
            return Ok(Some(snark));
        }
        submit(&*chain, &pid31, &snark, max_total)
            .await
            .map(|()| None)
    };
    let msg = match attempt.await {
        Ok(Some(snark)) => {
            info!(pid = %hex::encode(pid31), "results proved; held until the grace ends");
            Msg::ResultsHeld {
                generation,
                root: expected_root,
                snark: Box::new(snark),
            }
        }
        Ok(None) => {
            info!(pid = %hex::encode(pid31), "results settled on-chain");
            Msg::FinalizeDone {
                generation,
                result: Ok(()),
            }
        }
        Err(f) => Msg::FinalizeDone {
            generation,
            result: Err(f.into()),
        },
    };
    let _ = tx.send(msg).await;
}

/// Submits a held results snark (the window re-checked first).
pub(crate) async fn run_submit_held(
    chain: Arc<dyn Chain>,
    pid31: [u8; 31],
    snark: PlonkSnark,
    max_total: u64,
    generation: u64,
    tx: mpsc::Sender<Msg>,
) {
    let result = match submit(&*chain, &pid31, &snark, max_total).await {
        Ok(()) => {
            info!(pid = %hex::encode(pid31), "held results settled on-chain");
            Ok(())
        }
        Err(f) => Err(f.into()),
    };
    let _ = tx.send(Msg::FinalizeDone { generation, result }).await;
}

enum Fail {
    Transient(String),
    Permanent(String),
    /// DKG plaintexts not there yet: poll again, not an error.
    Wait(String),
}

impl From<Fail> for FinalizeFail {
    fn from(f: Fail) -> Self {
        let (permanent, wait, msg) = match f {
            Fail::Transient(m) => (false, false, m),
            Fail::Permanent(m) => (true, false, m),
            Fail::Wait(m) => (false, true, m),
        };
        FinalizeFail {
            permanent,
            wait,
            msg,
            dkg_requested: None,
        }
    }
}

/// The `requestResultsDecryption` arguments: 64 BE coordinates and the
/// siblings, root to leaf.
pub(crate) type DkgInputs = ([[u8; 32]; 64], Vec<[u8; 32]>);

/// Longest random pause before a DKG call. Every signing node reaches the
/// same call on the same heartbeat; staggered, the first one's transaction
/// shows in the others' fresh read and they skip theirs.
const DKG_JITTER: Duration = Duration::from_secs(10);

async fn jitter() {
    let ms = rand::thread_rng().gen_range(0..DKG_JITTER.as_millis() as u64);
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// One DKG finalize pass: request the decryption unless it was, then
/// publish the plaintexts once the committee combined them all. Each call
/// goes out after a random pause and only if a fresh read still lacks it;
/// another node winning either step counts as done. `inputs` is `None` when
/// the actor already saw the request on-chain. Reports through the mailbox.
pub(crate) async fn run_finalize_dkg(
    chain: Arc<dyn Chain>,
    pid31: [u8; 31],
    inputs: Option<DkgInputs>,
    expected_root: [u8; 32],
    max_total: u64,
    generation: u64,
    tx: mpsc::Sender<Msg>,
) {
    let mut seen = None;
    let attempt = dkg_attempt(
        &*chain,
        &pid31,
        inputs.as_ref(),
        &expected_root,
        max_total,
        &mut seen,
    );
    let result = match attempt.await {
        Ok(()) => {
            info!(pid = %hex::encode(pid31), "DKG results on-chain");
            Ok(())
        }
        Err(f) => {
            let mut f = FinalizeFail::from(f);
            f.dkg_requested = seen;
            Err(f)
        }
    };
    let _ = tx.send(Msg::FinalizeDone { generation, result }).await;
}

// `seen` gets the request flag as last observed on-chain.
async fn dkg_attempt(
    chain: &dyn Chain,
    pid31: &[u8; 31],
    inputs: Option<&DkgInputs>,
    expected_root: &[u8; 32],
    max_total: u64,
    seen: &mut Option<bool>,
) -> Result<(), Fail> {
    let transient = |e: Web3Error| Fail::Transient(e.to_string());
    let (_, now) = chain.head().await.map_err(transient)?;
    let p = chain.process(pid31).await.map_err(transient)?;
    match p.status {
        ProcessStatus::Results => return Ok(()),
        ProcessStatus::Canceled => {
            return Err(Fail::Transient(
                "process is Canceled; not finalizing".into(),
            ));
        }
        _ if now >= p.grace_end(max_total) => {}
        _ => return Err(Fail::Transient("grace window still open".into())),
    }
    *seen = Some(p.dkg.requested);
    if !p.dkg.requested {
        // The actor saw a request the chain no longer has (reorg): report
        // it through `seen`, the next attempt rebuilds the inputs.
        let Some((accumulator, siblings)) = inputs else {
            return Err(Fail::Transient(
                "the DKG request is gone; rebuilding".into(),
            ));
        };
        // The siblings prove the leaf under our root only.
        if p.state_root != *expected_root {
            return Err(Fail::Transient(
                "on-chain root is not the local committed root".into(),
            ));
        }
        // Send only if another node's request has not landed meanwhile.
        jitter().await;
        let p = chain.process(pid31).await.map_err(transient)?;
        if p.status == ProcessStatus::Results {
            return Ok(());
        }
        if !p.dkg.requested {
            match chain
                .request_results_decryption(pid31, accumulator, siblings)
                .await
            {
                Ok(r) => {
                    info!(pid = %hex::encode(pid31), tx = %r.tx_hash, "DKG decryption requested")
                }
                Err(Web3Error::Revert(r)) if r.name() == Some("ResultsAlreadyRequested") => {}
                Err(e) => return dkg_fail(chain, pid31, e, "request").await,
            }
        }
        *seen = Some(true);
        // No active field: the request itself finalized.
        if chain.process(pid31).await.map_err(transient)?.status == ProcessStatus::Results {
            return Ok(());
        }
    }
    if !chain.dkg_results_ready(pid31).await.map_err(transient)? {
        return Err(Fail::Wait("DKG plaintexts not combined yet".into()));
    }
    // The same for the finalize: pause, then send unless another node's
    // has landed.
    jitter().await;
    if chain.process(pid31).await.map_err(transient)?.status == ProcessStatus::Results {
        return Ok(());
    }
    match chain.finalize_results_from_dkg(pid31).await {
        Ok(_) => Ok(()),
        Err(Web3Error::Revert(r)) if r.name() == Some("ResultsNotReady") => {
            Err(Fail::Wait(format!("finalize: {r}")))
        }
        Err(e) => dkg_fail(chain, pid31, e, "finalize").await,
    }
}

// A DKG call failed. RESULTS already set (another node won) is done. A
// race lost to another node's call (`InvalidStatus` once it finalized, or a
// revert the replay cannot name) is no error: poll again. A window revert
// (an extension, or a landing that moved the grace end) retries; any other
// revert latches.
async fn dkg_fail(
    chain: &dyn Chain,
    pid31: &[u8; 31],
    e: Web3Error,
    what: &str,
) -> Result<(), Fail> {
    let (race, window) = match &e {
        Web3Error::Revert(r) => (
            r.name() == Some("InvalidStatus"),
            matches!(r.name(), Some("InvalidTimeBounds" | "GraceOpen")),
        ),
        Web3Error::Lost { .. } => (true, false),
        _ => (false, false),
    };
    if (race || window)
        && chain
            .process(pid31)
            .await
            .is_ok_and(|p| p.status == ProcessStatus::Results)
    {
        return Ok(());
    }
    Err(match e {
        e if race => Fail::Wait(format!("{what} lost a race: {e}")),
        e if window => Fail::Transient(format!("{what} reverted: {e}")),
        Web3Error::Revert(r) => Fail::Permanent(format!("{what} reverted: {r}")),
        e @ Web3Error::NoSigner => Fail::Permanent(e.to_string()),
        e => Fail::Transient(format!("{what}: {e}")),
    })
}

// The organizer can still extend a time-closed election, a late landing
// moves the grace end, and results may already be on-chain. Re-check the
// window before burning GPU time on the proof and again right before the
// plaintext tally meets the mempool.
async fn window_still_closed(
    chain: &dyn Chain,
    pid31: &[u8; 31],
    max_total: u64,
) -> Result<(), Fail> {
    let (_, now) = chain
        .head()
        .await
        .map_err(|e| Fail::Transient(e.to_string()))?;
    let p = chain
        .process(pid31)
        .await
        .map_err(|e| Fail::Transient(e.to_string()))?;
    match p.status {
        // Results landed (ours or another's) or the process died: nothing
        // to broadcast. Transient, the actor's own gates stop the retries.
        ProcessStatus::Results | ProcessStatus::Canceled => Err(Fail::Transient(format!(
            "process is {:?}; not broadcasting results",
            p.status
        ))),
        _ if now >= p.grace_end(max_total) => Ok(()),
        _ => Err(Fail::Transient("grace window still open".into())),
    }
}

// Proves the results and checks the snark against the host's own view.
async fn prove_checked(
    prover: &dyn Prover,
    request: &ResultsRequest,
    expected_root: &[u8; 32],
    tally: &[u64; NUM_FIELDS],
) -> Result<PlonkSnark, Fail> {
    // A `failed` proving job fails the same way on every retry; only
    // transport/queue trouble is worth a new attempt.
    let (got, snark) = prover.prove_results(request).await.map_err(|e| {
        if e.permanent {
            Fail::Permanent(e.to_string())
        } else {
            Fail::Transient(e.to_string())
        }
    })?;
    if !got.ok || got.fail_mask != 0 {
        let bits = results_fail_bits(got.fail_mask).join(", ");
        return Err(Fail::Permanent(format!(
            "results guest rejected: {bits} (mask {:#x}, cp index {})",
            got.fail_mask, got.cp_fail_index
        )));
    }
    if got.state_root != *expected_root {
        return Err(Fail::Permanent("results proof is for another root".into()));
    }
    if got.results != *tally {
        return Err(Fail::Permanent(
            "proved results differ from the host tally".into(),
        ));
    }
    match ResultsPublics::from_public_values(&snark.public_values) {
        Ok(pv) if pv == got => {}
        _ => return Err(Fail::Permanent("snark public values mismatch".into())),
    }
    if snark.program_vk != release::RESULTS_PROGRAM_VK {
        return Err(Fail::Permanent(
            "program vk is not the pinned results circuit vk".into(),
        ));
    }
    if snark.root_c_vadcop_final != release::ROOT_C_VADCOP_FINAL {
        return Err(Fail::Permanent(
            "root_c_vadcop_final is not the pinned setup".into(),
        ));
    }
    Ok(snark)
}

// Submits checked results once the window is still closed.
async fn submit(
    chain: &dyn Chain,
    pid31: &[u8; 31],
    snark: &PlonkSnark,
    max_total: u64,
) -> Result<(), Fail> {
    // An extension or a late landing may have moved the window meanwhile.
    window_still_closed(chain, pid31, max_total).await?;
    match chain.submit_results(pid31, snark).await {
        Ok(_) => Ok(()),
        // Window reverts mean an extension or a late landing raced the
        // broadcast, and a moved root a late foreign transition: transient,
        // the actor resyncs and re-evaluates after its cooldown.
        Err(Web3Error::Revert(r))
            if matches!(
                r.name(),
                Some(
                    "InvalidStatus"
                        | "InvalidTimeBounds"
                        | "ProcessNotEnded"
                        | "GraceOpen"
                        | "InvalidStateRoot"
                )
            ) =>
        {
            Err(Fail::Transient(format!("results reverted: {r}")))
        }
        Err(Web3Error::Revert(r)) => Err(Fail::Permanent(format!("results reverted: {r}"))),
        // An observer can never submit; retrying would loop forever.
        Err(e @ Web3Error::NoSigner) => Err(Fail::Permanent(e.to_string())),
        // Everything else re-arms, a mined revert the replay cannot name
        // (`Lost`) included.
        Err(e) => Err(Fail::Transient(e.to_string())),
    }
}
