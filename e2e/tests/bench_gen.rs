//! Offline ballot generation and merge of the benchmark, gated by
//! `CIRCOM_ARTIFACTS`. Proves real circom ballots on the CPU.

use davinci_e2e::bench::ballots::{self, SHARD};
use davinci_e2e::bench::r#gen::{GenOpts, gen_wave};
use davinci_e2e::bench::plan::{ElectionSpec, NodeId, Plan, Seed};
use davinci_e2e::fixture as fx;
use davinci_zkvm_sdk::ballot::address_to_fr;
use davinci_zkvm_sdk::census::{eth_address, vote_id_recover};
use davinci_zkvm_sdk::groth16::BallotVerifier;
use davinci_zkvm_sdk::release;

fn spec(tag: &str, nf: u8, origin: u8, voters: u32, rounds: u8, pm: u16) -> ElectionSpec {
    ElectionSpec {
        tag: tag.into(),
        scenario: "t".into(),
        nf,
        origin,
        voters,
        rounds,
        revote_permille: pm,
        key_node: NodeId::A,
        grace: 60,
    }
}

#[test]
fn gen_two_hosts_offline() {
    let Some(artifacts) = std::env::var_os("CIRCOM_ARTIFACTS") else {
        eprintln!("skipping gen_two_hosts_offline: CIRCOM_ARTIFACTS is not set");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let camp = dir.path();
    let seed = Seed::create(&camp.join("seed")).unwrap();
    let plan = Plan::stand_in(
        &seed,
        1,
        &[spec("G2", 2, 1, 6, 2, 500), spec("G16", 16, 4, 4, 1, 0)],
    )
    .unwrap();
    let prover = fx::load_prover().unwrap();
    let opts = |host| GenOpts {
        host,
        hosts: 2,
        threads: 4,
        host_name: format!("h{host}"),
        artifacts: artifacts.clone().into(),
    };
    let t = std::time::Instant::now();
    let mut ballots_n = 0;
    for h in 0..2 {
        let st = gen_wave(camp, &plan, 1, &seed, &prover, &opts(h)).unwrap();
        assert_eq!(st.skipped, 0);
        ballots_n += st.ballots;
    }
    eprintln!(
        "proved {ballots_n} ballots in {:.1}s",
        t.elapsed().as_secs_f64()
    );
    let wave = plan.wave(1).unwrap();
    let all = ballots::shards(wave, &seed, SHARD);
    let v = BallotVerifier::from_snarkjs_json(release::ballot_vk_json()).unwrap();
    let mut seen = 0;
    for s in &all {
        let p = ballots::shard_path(camp, 1, s);
        assert!(ballots::done(&p), "{s:?}");
        let (_, recs) = ballots::read_shard(&p).unwrap();
        for r in recs {
            let q = &r.req;
            let pubs = [
                address_to_fr(&q.address),
                davinci_client::api::Fr::from(q.vote_id),
                q.ballot_inputs_hash,
            ];
            assert!(v.verify(&q.ballot_proof, &pubs), "{} v{}", r.tag, r.voter);
            let want = eth_address(seed.voter_key(&r.tag, r.voter).verifying_key());
            assert_eq!(vote_id_recover(q.vote_id, &q.signature()).unwrap(), want);
            assert_eq!(q.address, want);
            seen += 1;
        }
    }
    assert_eq!(seen, ballots_n);
    for h in 0..2 {
        let st = gen_wave(camp, &plan, 1, &seed, &prover, &opts(h)).unwrap();
        assert_eq!((st.skipped, st.ballots), (st.shards, 0));
    }
}
