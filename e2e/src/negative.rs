//! Free negative checks against the registry through `eth_call`: settled
//! transactions replayed or tampered with, simulated at the block before
//! the original so everything but the change still holds.

use alloy::consensus::Transaction as _;
use alloy::eips::BlockId;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, B256, Bytes, FixedBytes, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use anyhow::{Context, Result, bail};
use davinci_client::organizer::{DAVINCITypes, ProcessRegistry as PR, RegistryReader, revert_name};

use crate::chain;
use crate::dkg;

/// Byte of word 10 in the 512-byte publics: the batch guest's root after,
/// the results guest's first tally word. Only the verifier reads it.
const FLIP_AT: usize = 80;

/// What one simulated call did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// Reverted with this error name.
    Reverted(String),
    /// Reverted, but the RPC stripped the revert data.
    Opaque(String),
    /// Not a revert: the RPC refused the call.
    Rpc(String),
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Ok => write!(f, "ok"),
            Outcome::Reverted(n) => write!(f, "reverted {n}"),
            Outcome::Opaque(m) => write!(f, "reverted without data ({m:.80})"),
            Outcome::Rpc(e) => write!(f, "rpc error {e:.160}"),
        }
    }
}

pub struct Check {
    pub name: &'static str,
    pub got: Outcome,
    /// `None`: passed; `Some(why)`: failed or, with `inconclusive`, not
    /// decidable on this RPC.
    pub problem: Option<String>,
    pub inconclusive: bool,
}

impl std::fmt::Display for Check {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let verdict = match (&self.problem, self.inconclusive) {
            (None, _) => "PASS".to_string(),
            (Some(w), true) => format!("INCONCLUSIVE ({w})"),
            (Some(w), false) => format!("FAIL ({w})"),
        };
        write!(
            f,
            "{:<34} {:<28} {verdict}",
            self.name,
            self.got.to_string()
        )
    }
}

/// Passes on a revert named in `want`. A revert without data or an RPC
/// error (pruned history, an unsupported field) cannot decide it.
fn judge(name: &'static str, got: Outcome, want: &[&str]) -> Check {
    let problem = match &got {
        Outcome::Reverted(n) if want.contains(&n.as_str()) => None,
        other => Some(format!("want {}, got {other}", want.join("/"))),
    };
    let inconclusive = undecided(&got);
    Check {
        name,
        got,
        problem,
        inconclusive,
    }
}

/// The untampered call must go through.
fn control(name: &'static str, got: Outcome) -> Check {
    Check {
        name,
        problem: (got != Outcome::Ok).then(|| format!("want ok, got {got}")),
        inconclusive: undecided(&got),
        got,
    }
}

fn undecided(got: &Outcome) -> bool {
    matches!(got, Outcome::Opaque(_) | Outcome::Rpc(_))
}

struct Sim<P> {
    p: P,
    /// The contract every call goes to.
    to: Address,
}

impl<P: Provider> Sim<P> {
    async fn call(
        &self,
        from: Address,
        input: Vec<u8>,
        blobs: Option<(Vec<B256>, u128)>,
        at: BlockId,
    ) -> Outcome {
        let mut tx = TransactionRequest::default()
            .with_from(from)
            .with_to(self.to)
            .with_input(Bytes::from(input));
        if let Some((hashes, fee)) = blobs {
            tx.blob_versioned_hashes = Some(hashes);
            tx.max_fee_per_blob_gas = Some(fee);
        }
        match self.p.call(tx).block(at).await {
            Ok(_) => Outcome::Ok,
            Err(e) => match e.as_error_resp() {
                Some(p) => match p.as_revert_data() {
                    Some(d) => Outcome::Reverted(revert_name(&d)),
                    None if p.message.contains("revert") => Outcome::Opaque(p.message.to_string()),
                    None => Outcome::Rpc(p.message.to_string()),
                },
                None => Outcome::Rpc(e.to_string()),
            },
        }
    }
}

/// `input` from `from` to `to`, simulated at the latest block.
pub async fn simulate(rpc: &str, from: Address, to: Address, input: Vec<u8>) -> Result<Outcome> {
    let p = ProviderBuilder::new().connect_client(chain::rpc(rpc)?);
    Ok(Sim { p, to }
        .call(from, input, None, BlockId::latest())
        .await)
}

/// Picks the latest transition since `from` and the latest results of an
/// origin-1 process (`pid`, if given), so the history they replay against
/// is shallow, and runs every check. Fails when a decidable check fails;
/// what the RPC cannot decide is INCONCLUSIVE.
pub async fn run(
    rpc: &str,
    registry: Address,
    from: u64,
    pid: Option<[u8; 31]>,
) -> Result<Vec<Check>> {
    let p = ProviderBuilder::new().connect_client(chain::rpc(rpc)?);
    let reader = RegistryReader::connect(rpc, registry)?;
    let results = chain::results_txs(rpc, registry, from).await?;
    let mut target = None;
    for r in results.iter().rev() {
        if pid.is_some_and(|x| x != r.pid) {
            continue;
        }
        let proc = reader.process(&r.pid).await?;
        // A DKG process has no results proof to tamper with.
        if proc.census_origin == 1 && proc.dkg.is_none() {
            target = Some((r.clone(), proc));
            break;
        }
    }
    let (res, proc) = target.context("no origin-1 process with results to check against")?;
    let pid = res.pid;
    let tr = chain::transitions(rpc, registry, from)
        .await?
        .pop()
        .context("no transition to check against")?;
    let organizer = Address::from(proc.organization_id);
    let sim = Sim { p, to: registry };
    let mut out = Vec::new();

    // The transition.
    let tx = sim
        .p
        .get_transaction_by_hash(tr.tx)
        .await?
        .context("transition tx not found")?;
    let from_tr = tx.inner.signer();
    let input = tx.inner.input().to_vec();
    let hashes = tx
        .inner
        .blob_versioned_hashes()
        .unwrap_or_default()
        .to_vec();
    let fee = tx.inner.max_fee_per_blob_gas().unwrap_or(1_000_000_000);
    let mut blobs = Some((hashes.clone(), fee));
    let (before, at) = (BlockId::number(tr.block - 1), BlockId::number(tr.block));
    let call = PR::submitStateTransitionCall::abi_decode(&input).context("decode transition")?;

    // Whether this RPC simulates blob hashes. Refused fields: retry without
    // them. MissingBlob: it ignores them. Either way the rest runs without
    // blob fields (the flips reach the verifier before the openings) and the
    // extra-hash check is INCONCLUSIVE.
    let mut got = sim
        .call(from_tr, input.clone(), blobs.clone(), before)
        .await;
    if matches!(got, Outcome::Rpc(_)) {
        blobs = None;
        got = sim.call(from_tr, input.clone(), None, before).await;
    }
    let mut chk = control("transition control at N-1", got);
    if matches!(&chk.got, Outcome::Reverted(n) if n == "MissingBlob") {
        blobs = None;
        chk.inconclusive = true;
        chk.problem = Some("the RPC does not simulate blob hashes".into());
    }
    let blob_sim = chk.got == Outcome::Ok && blobs.is_some();
    out.push(chk);
    let replay = sim.call(from_tr, input.clone(), blobs.clone(), at).await;
    out.push(judge("replay at N", replay, &["InvalidStateRoot"]));

    let mut c = call.clone();
    let mut proof = c.proofBytes.to_vec();
    let mid = proof.len() / 2;
    proof[mid] ^= 1;
    c.proofBytes = proof.into();
    let got = sim
        .call(from_tr, c.abi_encode(), blobs.clone(), before)
        .await;
    out.push(judge("proof byte flipped at N-1", got, &["InvalidProof"]));

    let mut c = call.clone();
    let mut pubs = c.publicValues.to_vec();
    pubs[FLIP_AT] ^= 1;
    c.publicValues = pubs.into();
    let got = sim
        .call(from_tr, c.abi_encode(), blobs.clone(), before)
        .await;
    out.push(judge(
        "root-after byte flipped at N-1",
        got,
        &["InvalidProof"],
    ));

    let mut c = call.clone();
    c.commitments.push(c.commitments[0].clone());
    c.ys.push(c.ys[0]);
    c.kzgProofs.push(c.kzgProofs[0].clone());
    let got = sim
        .call(from_tr, c.abi_encode(), blobs.clone(), before)
        .await;
    out.push(judge(
        "extra blob in the arrays at N-1",
        got,
        &["BlobCountMismatch"],
    ));

    let mut more = hashes.clone();
    let mut extra = B256::with_last_byte(1);
    extra.0[0] = 1; // the KZG version byte
    more.push(extra);
    if blob_sim {
        let got = sim
            .call(from_tr, input.clone(), Some((more, fee)), before)
            .await;
        out.push(judge("extra blob hash at N-1", got, &["BlobCountMismatch"]));
    } else {
        out.push(Check {
            name: "extra blob hash at N-1",
            got: Outcome::Rpc("skipped".into()),
            problem: Some("the RPC does not simulate blob hashes".into()),
            inconclusive: true,
        });
    }

    // The results.
    let rtx = sim
        .p
        .get_transaction_by_hash(res.tx)
        .await?
        .context("results tx not found")?;
    let from_r = rtx.inner.signer();
    let rin = rtx.inner.input().to_vec();
    let rbefore = BlockId::number(res.block - 1);
    let got = sim.call(from_r, rin.clone(), None, rbefore).await;
    out.push(control("results control at R-1", got));
    let mut c = PR::setProcessResultsCall::abi_decode(&rin).context("decode results")?;
    let mut pubs = c.publicValues.to_vec();
    pubs[FLIP_AT] ^= 1;
    c.publicValues = pubs.into();
    let got = sim.call(from_r, c.abi_encode(), None, rbefore).await;
    out.push(judge(
        "results tally tampered at R-1",
        got,
        &["InvalidProof"],
    ));

    // Organizer calls on the finished process.
    let c = PR::setProcessDurationCall {
        processId: FixedBytes(pid),
        _duration: U256::from(proc.duration * 2),
    };
    let got = sim
        .call(organizer, c.abi_encode(), None, BlockId::latest())
        .await;
    out.push(judge(
        "setProcessDuration after the end",
        got,
        &["InvalidStatus", "InvalidTimeBounds"],
    ));
    let c = PR::setProcessCensusCall {
        processId: FixedBytes(pid),
        census: DAVINCITypes::Census {
            censusOrigin: 2,
            censusRoot: B256::with_last_byte(1),
            contractAddress: Address::ZERO,
            censusURI: "file:///x".into(),
            onchainAllowAnyValidRoot: false,
        },
    };
    let got = sim
        .call(organizer, c.abi_encode(), None, BlockId::latest())
        .await;
    out.push(judge(
        "setProcessCensus on origin 1",
        got,
        &["CensusNotUpdatable"],
    ));
    eprintln!(
        "negative checks on {} (transition {} at {}, results {} at {}):",
        davinci_client::api::ProcessId(pid),
        tr.tx,
        tr.block,
        res.tx,
        res.block
    );
    verdict(out)
}

/// Prints every check; fails when a decidable one failed.
fn verdict(out: Vec<Check>) -> Result<Vec<Check>> {
    for c in &out {
        eprintln!("  {c}");
    }
    let failed: Vec<_> = out
        .iter()
        .filter(|c| c.problem.is_some() && !c.inconclusive)
        .map(|c| c.name)
        .collect();
    if !failed.is_empty() {
        bail!("negative checks failed: {}", failed.join(", "));
    }
    Ok(out)
}

/// `BabyJubJub` base field modulus (BN254 r): accumulator coordinates must
/// be below it.
const Q: &str = "21888242871839275222246405745257275088548364400416034343698204186575808495617";

/// The DKG results path, replayed and tampered with around the two settled
/// requests: `auto` (a process ended by its organizer) and `locked` (one that
/// ran out its duration, so its request is what ended it). Also the
/// registrar gate on `app_manager`, the zkVM results path on a DKG process,
/// and the organizer's cancel on either side of `locked`'s request.
pub async fn dkg(
    rpc: &str,
    registry: Address,
    app_manager: Address,
    organizer: Address,
    auto: &chain::RegistryTx,
    locked: &chain::RegistryTx,
) -> Result<Vec<Check>> {
    let p = ProviderBuilder::new().connect_client(chain::rpc(rpc)?);
    let sim = Sim { p, to: registry };
    let mut out = Vec::new();
    let sp = &sim.p;
    let tx = |h| async move {
        sp.get_transaction_by_hash(h)
            .await?
            .context("request tx not found")
    };
    let (a_tx, l_tx) = (tx(auto.tx).await?, tx(locked.tx).await?);
    let from = a_tx.inner.signer();
    let a_in = a_tx.inner.input().to_vec();
    let a = PR::requestResultsDecryptionCall::abi_decode(&a_in).context("decode request")?;
    let l = PR::requestResultsDecryptionCall::abi_decode(l_tx.inner.input())
        .context("decode request")?;
    let (before, at) = (BlockId::number(auto.block - 1), BlockId::number(auto.block));

    let got = sim.call(from, a_in.clone(), None, before).await;
    out.push(control("request control at A-1", got));
    let mut c = a.clone();
    c.accumulator[2] ^= U256::from(1);
    let got = sim.call(from, c.abi_encode(), None, before).await;
    out.push(judge(
        "accumulator coordinate +-1 at A-1",
        got,
        &["InvalidInclusionProof"],
    ));
    let mut c = a.clone();
    c.accumulator[0] = Q.parse()?;
    let got = sim.call(from, c.abi_encode(), None, before).await;
    out.push(judge(
        "accumulator coordinate = Q at A-1",
        got,
        &["InvalidAccumulator"],
    ));
    let mut c = a.clone();
    let i = c
        .siblings
        .iter()
        .position(|x| !x.is_zero())
        .context("no non-zero sibling")?;
    c.siblings[i].0[31] ^= 1;
    let got = sim.call(from, c.abi_encode(), None, before).await;
    out.push(judge(
        "sibling byte flipped at A-1",
        got,
        &["InvalidInclusionProof"],
    ));
    let mut c = a.clone();
    (c.accumulator, c.siblings) = (l.accumulator, l.siblings.clone());
    let got = sim.call(from, c.abi_encode(), None, before).await;
    out.push(judge(
        "another process's accumulator at A-1",
        got,
        &["InvalidInclusionProof"],
    ));
    let got = sim.call(from, a_in, None, BlockId::latest()).await;
    out.push(judge("second request", got, &["ResultsAlreadyRequested"]));
    let c = PR::finalizeResultsFromDKGCall {
        processId: a.processId,
    };
    let got = sim.call(from, c.abi_encode(), None, at).await;
    out.push(judge(
        "finalize at A, before the combines",
        got,
        &["ResultsNotReady"],
    ));

    // The request closes the organizer's veto on a process past its end.
    let cancel = PR::setProcessStatusCall {
        processId: l.processId,
        newStatus: 2,
    }
    .abi_encode();
    let got = sim
        .call(
            organizer,
            cancel.clone(),
            None,
            BlockId::number(locked.block - 1),
        )
        .await;
    out.push(control("cancel before the request (L-1)", got));
    let got = sim
        .call(organizer, cancel, None, BlockId::number(locked.block))
        .await;
    out.push(judge(
        "cancel after the request (L)",
        got,
        &["InvalidStatus"],
    ));

    let c = PR::setProcessResultsCall {
        processId: a.processId,
        publicValues: Bytes::new(),
        proofBytes: Bytes::new(),
    };
    let got = sim
        .call(from, c.abi_encode(), None, BlockId::latest())
        .await;
    out.push(judge(
        "setProcessResults on a DKG process",
        got,
        &["InvalidKeyMode"],
    ));
    let am = Sim {
        p: ProviderBuilder::new().connect_client(chain::rpc(rpc)?),
        to: app_manager,
    };
    let got = am
        .call(
            organizer,
            dkg::register_app_call(FixedBytes::ZERO),
            None,
            BlockId::latest(),
        )
        .await;
    out.push(judge(
        "registerApplication from an EOA",
        got,
        &["NotRegistrar"],
    ));
    eprintln!(
        "DKG negative checks (requests {} at {}, {} at {}):",
        auto.tx, auto.block, locked.tx, locked.block
    );
    verdict(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts() {
        let r = |n: &str| Outcome::Reverted(n.into());
        let pass = judge("x", r("InvalidProof"), &["InvalidProof"]);
        assert!(pass.problem.is_none());
        let wrong = judge("x", r("InvalidStateRoot"), &["InvalidProof"]);
        assert!(wrong.problem.is_some() && !wrong.inconclusive);
        let ok = judge("x", Outcome::Ok, &["InvalidProof"]);
        assert!(ok.problem.is_some() && !ok.inconclusive);
        for o in [
            Outcome::Opaque("execution reverted".into()),
            Outcome::Rpc("missing trie node".into()),
        ] {
            let c = judge("x", o.clone(), &["InvalidProof"]);
            assert!(c.problem.is_some() && c.inconclusive);
            let c = control("x", o);
            assert!(c.problem.is_some() && c.inconclusive);
        }
        assert!(control("x", Outcome::Ok).problem.is_none());
        let c = control("x", r("InvalidStateRoot"));
        assert!(c.problem.is_some() && !c.inconclusive);
    }
}
