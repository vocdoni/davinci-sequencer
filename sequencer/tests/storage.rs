//! redb storage: every table round-trips, pending is FIFO, statuses move,
//! data survives a reopen, a schema mismatch is refused, and each deployment
//! keeps its own database under the datadir.

use davinci_sequencer::keys::KeyStore;
use davinci_sequencer::storage::{
    DB_FILE, DEFAULT_CACHE_MB, Db, ExposedRecord, L1PostRecord, LocalStatus, ProcessRecord,
    SCHEMA_VERSION_KEY, StorageError, StoredVote, TransitionRecord, VoteStatus, deployment_dir,
    pid_prefix,
};
use davinci_sequencer::web3::L1Fees;
use davinci_sequencer::web3::{DkgState, KeyMode, OnchainCensus, OnchainProcess, ProcessStatus};
use davinci_zkvm_sdk::ballot::BallotMode;
use davinci_zkvm_sdk::blob::{BLOB_SIZE, Blob};
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::crypto::field::Fr;

fn pid(n: u64) -> Fr {
    Fr::from(n)
}

fn onchain(root: u8) -> OnchainProcess {
    OnchainProcess {
        status: ProcessStatus::Ready,
        organizer: [7u8; 20],
        enc_key: Point::generator(),
        state_root: [root; 32],
        results: vec![],
        start_time: 100,
        duration: 3600,
        max_voters: 50,
        voters_count: 0,
        overwritten_count: 0,
        creation_block: 9,
        batch_number: 0,
        metadata_uri: "ipfs://m".into(),
        metadata_hash: [6u8; 32],
        ballot_mode: BallotMode {
            num_fields: 4,
            group_size: 1,
            unique_values: false,
            cost_exponent: 1,
            max_value: 3,
            min_value: 0,
            max_value_sum: 12,
            min_value_sum: 0,
        },
        census: OnchainCensus {
            origin: 1,
            root: [5u8; 32],
            uri: "file:///tmp/c.json".into(),
            contract_address: [0u8; 20],
        },
        // Non-default DKG fields, so the round-trips cover them.
        key_mode: KeyMode::DkgLocked,
        dkg: DkgState {
            epoch_id: [3u8; 12],
            aid: [4u8; 32],
            requested: true,
            first_index: 2,
            count: 3,
        },
        grace: 180,
        last_vote_at: 7200,
    }
}

// The registry's `_graceEnd`: min(end + maxTotal, max(end, lastVoteAt) + grace).
#[test]
fn grace_end_matches_the_registry() {
    let mut p = onchain(1);
    let end = p.end_time();
    assert_eq!(end, 3700);
    p.last_vote_at = 0; // no votes
    assert_eq!(p.grace_end(1800), end + 180);
    p.last_vote_at = end - 50; // landed before the end
    assert_eq!(p.grace_end(1800), end + 180);
    p.last_vote_at = end + 100; // landed in the grace
    assert_eq!(p.grace_end(1800), end + 280);
    p.last_vote_at = end + 1700; // capped
    assert_eq!(p.grace_end(1800), end + 1800);
    assert_eq!(p.grace_end(0), end);
    p.duration = u64::MAX - 100 - 10; // end + maxTotal overflows
    assert_eq!(p.grace_end(11), u64::MAX);
}

fn process(n: u64) -> ProcessRecord {
    ProcessRecord {
        pid: pid(n),
        onchain: onchain(n as u8),
        local: LocalStatus::Active,
        note: None,
    }
}

fn vote(p: u64, vid: u64) -> StoredVote {
    StoredVote {
        pid: pid(p),
        vote_id: vid,
        address: [vid as u8; 20],
        slot: 0x11 + vid % 7,
        package: vec![1, 2, 3, vid as u8],
        status: VoteStatus::Pending,
        error: None,
        created_at: 1000,
        updated_at: 1000,
    }
}

fn blob(fill: u8) -> Blob {
    let mut b: Blob = vec![0u8; BLOB_SIZE].into_boxed_slice().try_into().unwrap();
    b[0] = fill;
    b[BLOB_SIZE - 1] = fill;
    b
}

fn transition(index: u64) -> TransitionRecord {
    TransitionRecord {
        index,
        old_root: [index as u8; 32],
        new_root: [index as u8 + 1; 32],
        tx_hash: [0xaa; 32],
        block: 40 + index,
        sender: [3u8; 20],
        n_votes: 5,
        n_overwrites: 1,
        n_blobs: 2,
        by_self: index.is_multiple_of(2),
    }
}

fn open(dir: &tempfile::TempDir) -> Db {
    Db::open(&dir.path().join("sequencer.redb")).unwrap()
}

#[test]
fn every_table_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);

    // processes
    assert!(db.process(&pid(1)).unwrap().is_none());
    db.put_process(&process(1)).unwrap();
    db.put_process(&process(2)).unwrap();
    assert_eq!(db.process(&pid(1)).unwrap(), Some(process(1)));
    let all = db.processes().unwrap();
    assert_eq!(all, vec![process(1), process(2)]);

    // votes
    assert!(db.vote(&pid(1), 1 << 63).unwrap().is_none());
    let v = vote(1, (1 << 63) + 5);
    db.put_vote(&v).unwrap();
    assert_eq!(db.vote(&pid(1), v.vote_id).unwrap(), Some(v.clone()));
    // Same vote id under another process is a different row.
    assert!(db.vote(&pid(2), v.vote_id).unwrap().is_none());

    // transitions and blobs
    let blobs = vec![blob(1), blob(2)];
    db.put_transition(&pid(1), &transition(0), &blobs).unwrap();
    db.put_transition(&pid(1), &transition(1), &[blob(9)])
        .unwrap();
    db.put_transition(&pid(2), &transition(0), &[]).unwrap();
    assert_eq!(
        db.transitions(&pid(1)).unwrap(),
        vec![transition(0), transition(1)]
    );
    assert_eq!(db.transitions(&pid(2)).unwrap(), vec![transition(0)]);
    assert_eq!(db.blobs(&pid(1), 0).unwrap(), blobs);
    assert_eq!(db.blobs(&pid(1), 1).unwrap(), vec![blob(9)]);
    assert!(db.blobs(&pid(1), 7).unwrap().is_empty());

    // meta
    assert_eq!(db.meta_u64("last_block").unwrap(), None);
    db.set_meta_u64("last_block", 1234).unwrap();
    assert_eq!(db.meta_u64("last_block").unwrap(), Some(1234));

    // arbo storage: one table per process, isolated from each other.
    use arbo::Storage;
    let s1 = db.arbo_storage(&pid(1)).unwrap();
    let s2 = db.arbo_storage(&pid(2)).unwrap();
    let mut wb = arbo::WriteBatch::default();
    wb.put(b"k".to_vec(), b"v1".to_vec());
    s1.write(wb).unwrap();
    assert_eq!(s1.get(b"k").unwrap(), Some(b"v1".to_vec()));
    assert_eq!(s2.get(b"k").unwrap(), None);
}

#[test]
fn pending_is_fifo() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let base = 1u64 << 63;
    // Insertion order, not numeric order.
    for vid in [base + 9, base + 2, base + 5, base + 1] {
        db.push_pending(&pid(1), vid).unwrap();
    }
    db.push_pending(&pid(2), base + 100).unwrap();
    assert_eq!(
        db.pending(&pid(1)).unwrap(),
        vec![base + 9, base + 2, base + 5, base + 1]
    );
    db.remove_pending(&pid(1), &[base + 2, base + 1]).unwrap();
    assert_eq!(db.pending(&pid(1)).unwrap(), vec![base + 9, base + 5]);
    // Appended after a removal, still at the tail.
    db.push_pending(&pid(1), base + 2).unwrap();
    assert_eq!(
        db.pending(&pid(1)).unwrap(),
        vec![base + 9, base + 5, base + 2]
    );
    assert_eq!(db.pending(&pid(2)).unwrap(), vec![base + 100]);

    // A requeue goes to the head; a vid still queued keeps its place.
    for i in [7, 8] {
        db.put_vote(&vote(1, base + i)).unwrap();
    }
    db.put_vote(&vote(1, base + 5)).unwrap();
    db.requeue(&pid(1), &[base + 7, base + 5, base + 8])
        .unwrap();
    assert_eq!(
        db.pending(&pid(1)).unwrap(),
        vec![base + 7, base + 8, base + 9, base + 5, base + 2]
    );
    db.remove_pending(&pid(1), &[base + 7]).unwrap();
    db.push_pending(&pid(1), base + 7).unwrap();
    assert_eq!(
        db.pending(&pid(1)).unwrap(),
        vec![base + 8, base + 9, base + 5, base + 2, base + 7]
    );
    assert_eq!(db.pending(&pid(2)).unwrap(), vec![base + 100]);
}

#[test]
fn status_transition() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let base = 1u64 << 63;
    for i in 0..3 {
        db.put_vote(&vote(1, base + i)).unwrap();
    }
    db.set_status(&pid(1), &[base, base + 1], VoteStatus::Aggregated, None)
        .unwrap();
    db.set_status(
        &pid(1),
        &[base + 2],
        VoteStatus::Error,
        Some("process closed"),
    )
    .unwrap();
    let a = db.vote(&pid(1), base).unwrap().unwrap();
    assert_eq!(a.status, VoteStatus::Aggregated);
    assert_eq!(a.error, None);
    assert!(a.updated_at >= a.created_at);
    let e = db.vote(&pid(1), base + 2).unwrap().unwrap();
    assert_eq!(e.status, VoteStatus::Error);
    assert_eq!(e.error.as_deref(), Some("process closed"));
    // Moving on clears the old error.
    db.set_status(&pid(1), &[base + 2], VoteStatus::Pending, None)
        .unwrap();
    assert_eq!(db.vote(&pid(1), base + 2).unwrap().unwrap().error, None);
    // Unknown votes are an error and the batch is not applied.
    assert!(
        db.set_status(&pid(1), &[base, base + 77], VoteStatus::Settled, None)
            .is_err()
    );
    assert_eq!(
        db.vote(&pid(1), base).unwrap().unwrap().status,
        VoteStatus::Aggregated
    );
    // The restart scan finds the in-flight votes of this process only.
    db.put_vote(&vote(2, base + 9)).unwrap();
    db.set_status(&pid(2), &[base + 9], VoteStatus::Processed, None)
        .unwrap();
    db.set_status(&pid(1), &[base + 1], VoteStatus::Processed, None)
        .unwrap();
    let in_flight = [VoteStatus::Aggregated, VoteStatus::Processed];
    assert_eq!(
        db.vote_ids_with_status(&pid(1), &in_flight).unwrap(),
        [base, base + 1]
    );
    assert_eq!(
        db.vote_ids_with_status(&pid(1), &[VoteStatus::Pending])
            .unwrap(),
        [base + 2]
    );
    // Wire strings are davinci-node's.
    assert_eq!(
        serde_json::to_string(&VoteStatus::Aggregated).unwrap(),
        "\"aggregated\""
    );
    assert_eq!(VoteStatus::Settled.as_str(), "settled");
}

#[test]
fn reopen_keeps_data() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = open(&dir);
        db.put_process(&process(3)).unwrap();
        db.push_pending(&pid(3), 1 << 63).unwrap();
        db.set_meta_u64("last_block", 77).unwrap();
    }
    let db = open(&dir);
    assert_eq!(db.process(&pid(3)).unwrap(), Some(process(3)));
    assert_eq!(db.pending(&pid(3)).unwrap(), vec![1 << 63]);
    assert_eq!(db.meta_u64("last_block").unwrap(), Some(77));
}

#[test]
fn schema_mismatch_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = open(&dir);
        db.set_meta_u64(SCHEMA_VERSION_KEY, 999).unwrap();
    }
    let err = Db::open(&dir.path().join("sequencer.redb")).err().unwrap();
    assert!(
        matches!(err, StorageError::Schema { found: 999, .. }),
        "{err}"
    );
}

#[test]
fn file_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sequencer.redb");
    let _db = Db::open(&path).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn corrupt_rows_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sequencer.redb");
    let base = 1u64 << 63;
    {
        let db = open(&dir);
        db.put_process(&process(1)).unwrap();
        db.put_transition(&pid(1), &transition(0), &[blob(1)])
            .unwrap();
        db.push_pending(&pid(1), base).unwrap();
    }
    // Overwrite rows behind the Db's back.
    {
        let raw = redb::Database::create(&path).unwrap();
        let tx = raw.begin_write().unwrap();
        {
            let pkey = davinci_zkvm_sdk::crypto::field::fr_to_be(&pid(1));
            let def: redb::TableDefinition<&[u8], &[u8]> = redb::TableDefinition::new("processes");
            tx.open_table(def)
                .unwrap()
                .insert(pkey.as_slice(), b"{not json".as_slice())
                .unwrap();
            let mut bkey = pkey.to_vec();
            bkey.extend_from_slice(&0u64.to_be_bytes());
            bkey.extend_from_slice(&0u16.to_be_bytes());
            let def: redb::TableDefinition<&[u8], &[u8]> = redb::TableDefinition::new("blobs");
            tx.open_table(def)
                .unwrap()
                .insert(bkey.as_slice(), [1u8; 10].as_slice())
                .unwrap();
            let mut qkey = pkey.to_vec();
            qkey.extend_from_slice(&0u64.to_be_bytes());
            let def: redb::TableDefinition<&[u8], &[u8]> = redb::TableDefinition::new("pending");
            tx.open_table(def)
                .unwrap()
                .insert(qkey.as_slice(), [1u8; 3].as_slice())
                .unwrap();
        }
        tx.commit().unwrap();
    }
    let db = open(&dir);
    assert!(db.process(&pid(1)).is_err());
    assert!(db.processes().is_err());
    assert!(db.blobs(&pid(1), 0).is_err());
    assert!(db.pending(&pid(1)).is_err());
}

#[test]
fn duplicate_push_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let base = 1u64 << 63;
    for vid in [base + 1, base + 2, base + 1, base + 2, base + 3] {
        db.push_pending(&pid(1), vid).unwrap();
    }
    assert_eq!(
        db.pending(&pid(1)).unwrap(),
        vec![base + 1, base + 2, base + 3]
    );
    // Removed then pushed again: back at the tail, once.
    db.remove_pending(&pid(1), &[base + 1, base + 1]).unwrap();
    db.push_pending(&pid(1), base + 1).unwrap();
    db.push_pending(&pid(1), base + 1).unwrap();
    assert_eq!(
        db.pending(&pid(1)).unwrap(),
        vec![base + 2, base + 3, base + 1]
    );
}

#[test]
fn vote_lifecycle_is_atomic_and_durable() {
    let dir = tempfile::tempdir().unwrap();
    let base = 1u64 << 63;
    let (a, b, c) = (base + 10, base + 20, base + 30);
    let status = |db: &Db, v: u64| db.vote(&pid(1), v).unwrap().unwrap().status;

    {
        let db = open(&dir);
        for v in [a, b, c] {
            db.admit_vote(&vote(1, v)).unwrap();
        }
        // A second admit of the same vote id is refused.
        assert!(matches!(
            db.admit_vote(&vote(1, a)),
            Err(StorageError::VoteExists(v)) if v == a
        ));
    }
    let db = open(&dir);
    assert_eq!(db.pending(&pid(1)).unwrap(), vec![a, b, c]);
    assert_eq!(status(&db, a), VoteStatus::Pending);

    db.seal_batch(&pid(1), &[a, b]).unwrap();
    drop(db);
    let db = open(&dir);
    assert_eq!(db.pending(&pid(1)).unwrap(), vec![c]);
    assert_eq!(status(&db, a), VoteStatus::Aggregated);
    assert_eq!(status(&db, b), VoteStatus::Aggregated);

    // An unknown vote aborts the whole seal: nothing moves.
    assert!(db.seal_batch(&pid(1), &[c, base + 99]).is_err());
    assert_eq!(db.pending(&pid(1)).unwrap(), vec![c]);
    assert_eq!(status(&db, c), VoteStatus::Pending);

    db.set_status(&pid(1), &[a], VoteStatus::Error, Some("boom"))
        .unwrap();
    db.requeue(&pid(1), &[a, c]).unwrap();
    drop(db);
    let db = open(&dir);
    // c was still queued and keeps its place; a goes back to the head.
    assert_eq!(db.pending(&pid(1)).unwrap(), vec![a, c]);
    let va = db.vote(&pid(1), a).unwrap().unwrap();
    assert_eq!((va.status, va.error), (VoteStatus::Pending, None));

    // The pinned root moves with the transition.
    let mut rec = process(1);
    rec.onchain.state_root = transition(0).new_root;
    db.settle(&pid(1), &[a, c], &transition(0), &[blob(1), blob(2)], &rec)
        .unwrap();
    drop(db);
    let db = open(&dir);
    assert!(db.pending(&pid(1)).unwrap().is_empty());
    assert_eq!(status(&db, a), VoteStatus::Settled);
    assert_eq!(status(&db, c), VoteStatus::Settled);
    assert_eq!(status(&db, b), VoteStatus::Aggregated);
    assert_eq!(db.transitions(&pid(1)).unwrap(), vec![transition(0)]);
    assert_eq!(db.blobs(&pid(1), 0).unwrap(), vec![blob(1), blob(2)]);
    assert_eq!(db.process(&pid(1)).unwrap(), Some(rec.clone()));

    // A failed settle leaves the transition and the record out too.
    let mut next = rec.clone();
    next.onchain.state_root = transition(1).new_root;
    assert!(
        db.settle(&pid(1), &[base + 77], &transition(1), &[blob(5)], &next)
            .is_err()
    );
    assert_eq!(db.transitions(&pid(1)).unwrap().len(), 1);
    assert!(db.blobs(&pid(1), 1).unwrap().is_empty());
    assert_eq!(db.process(&pid(1)).unwrap(), Some(rec.clone()));

    // So does a write failing at commit: nothing of it persists.
    db.fail_next_writes(1);
    assert!(
        db.settle(&pid(1), &[], &transition(1), &[blob(5)], &next)
            .is_err()
    );
    assert_eq!(db.transitions(&pid(1)).unwrap().len(), 1);
    assert_eq!(db.process(&pid(1)).unwrap(), Some(rec));
    db.settle(&pid(1), &[], &transition(1), &[blob(5)], &next)
        .unwrap();
    assert_eq!(db.process(&pid(1)).unwrap(), Some(next));
}

#[test]
fn rewriting_a_transition_drops_stale_blobs() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    db.put_transition(&pid(1), &transition(4), &[blob(1), blob(2), blob(3)])
        .unwrap();
    db.put_transition(&pid(1), &transition(4), &[blob(7)])
        .unwrap();
    assert_eq!(db.blobs(&pid(1), 4).unwrap(), vec![blob(7)]);
}

#[test]
fn settled_vote_cannot_be_admitted_again() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let base = 1u64 << 63;
    let (a, b) = (base + 1, base + 2);
    db.admit_vote(&vote(1, a)).unwrap();
    db.admit_vote(&vote(1, b)).unwrap();
    db.seal_batch(&pid(1), &[a]).unwrap();
    db.settle(&pid(1), &[a], &transition(0), &[blob(1)], &process(1))
        .unwrap();
    let mut again = vote(1, a);
    again.package = vec![9, 9, 9];
    assert!(matches!(
        db.admit_vote(&again),
        Err(StorageError::VoteExists(_))
    ));
    drop(db);
    let db = open(&dir);
    let v = db.vote(&pid(1), a).unwrap().unwrap();
    assert_eq!(v.status, VoteStatus::Settled);
    assert_eq!(v.package, vote(1, a).package);
    assert_eq!(db.pending(&pid(1)).unwrap(), vec![b]);
    // The same vote id under another process is a different vote.
    db.admit_vote(&vote(2, a)).unwrap();
}

#[test]
fn exposed_record_roundtrips_as_one_union_list() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    assert_eq!(db.exposed(&pid(1)).unwrap(), None);
    let rec = ExposedRecord {
        slots: vec![3, 7, 9],
        vote_ids: vec![11, 12],
    };
    db.put_exposed(&pid(1), &rec).unwrap();
    assert_eq!(db.exposed(&pid(1)).unwrap(), Some(rec.clone()));
    assert_eq!(db.exposed(&pid(2)).unwrap(), None);
    // One slot list: nothing in the stored form tells a refreshed
    // slot from a written one.
    let json = serde_json::to_value(&rec).unwrap();
    let keys: Vec<&String> = json.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["slots", "vote_ids"]);
    // Survives a reopen.
    drop(db);
    let db = open(&dir);
    assert_eq!(db.exposed(&pid(1)).unwrap(), Some(rec));
    db.clear_exposed(&pid(1)).unwrap();
    assert_eq!(db.exposed(&pid(1)).unwrap(), None);
    // Clearing a missing record is a no-op.
    db.clear_exposed(&pid(1)).unwrap();
}

const REG_A: [u8; 20] = [0xaa; 20];
const REG_B: [u8; 20] = [0xbb; 20];

/// A process of registry `reg` on chain 100: creator, id prefix, nonce.
fn process_of(reg: &[u8; 20], nonce: u8) -> ProcessRecord {
    let mut be = [0u8; 32];
    be[1..21].copy_from_slice(&[7u8; 20]);
    be[21..25].copy_from_slice(&pid_prefix(100, reg).unwrap());
    be[31] = nonce;
    ProcessRecord {
        pid: davinci_zkvm_sdk::crypto::field::fr_from_be(&be).unwrap(),
        ..process(1)
    }
}

// A new registry on the same datadir starts empty; the old deployment's
// file stays as it was.
#[test]
fn another_registry_starts_fresh() {
    let dir = tempfile::tempdir().unwrap();
    let a = process_of(&REG_A, 1);
    {
        let db = Db::open_deployment(dir.path(), 100, &REG_A, DEFAULT_CACHE_MB).unwrap();
        db.put_process(&a).unwrap();
        db.set_meta_u64("monitor_last_block", 500).unwrap();
        KeyStore::open(&db, 100, REG_A).unwrap();
        assert_eq!(db.enc_key_count().unwrap(), 1);
    }
    let path_a = deployment_dir(dir.path(), 100, &REG_A).join(DB_FILE);
    let before = std::fs::read(&path_a).unwrap();
    assert!(path_a.ends_with(format!("100-0x{}/sequencer.redb", "aa".repeat(20))));

    let db = Db::open_deployment(dir.path(), 100, &REG_B, DEFAULT_CACHE_MB).unwrap();
    assert!(db.processes().unwrap().is_empty());
    assert_eq!(db.meta_u64("monitor_last_block").unwrap(), None);
    assert_eq!(db.enc_key_count().unwrap(), 0);
    drop(db);
    assert_eq!(std::fs::read(&path_a).unwrap(), before);

    // Back on the first registry, everything is where it was.
    let db = Db::open_deployment(dir.path(), 100, &REG_A, DEFAULT_CACHE_MB).unwrap();
    assert_eq!(db.processes().unwrap(), vec![a]);
    assert_eq!(db.meta_u64("monitor_last_block").unwrap(), Some(500));
    let mode = |p: &std::path::Path| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    };
    assert_eq!(mode(&deployment_dir(dir.path(), 100, &REG_B)), 0o700);
    assert_eq!(mode(&path_a), 0o600);
}

// A database copied into another deployment's directory is refused.
#[test]
fn a_database_is_bound_to_its_deployment() {
    let dir = tempfile::tempdir().unwrap();
    drop(Db::open_deployment(dir.path(), 100, &REG_A, DEFAULT_CACHE_MB).unwrap());
    std::fs::rename(
        deployment_dir(dir.path(), 100, &REG_A),
        deployment_dir(dir.path(), 100, &REG_B),
    )
    .unwrap();
    let err = Db::open_deployment(dir.path(), 100, &REG_B, DEFAULT_CACHE_MB)
        .err()
        .unwrap();
    assert!(matches!(err, StorageError::Deployment { .. }), "{err}");
}

// The flat layout (`<datadir>/sequencer.redb`) predates the coverage
// history: refused, with or without processes, and left in place.
#[test]
fn flat_layout_is_refused() {
    for with_process in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = Db::open_in(dir.path()).unwrap();
            if with_process {
                db.put_process(&process_of(&REG_A, 1)).unwrap();
            }
        }
        let err = Db::open_deployment(dir.path(), 100, &REG_A, DEFAULT_CACHE_MB)
            .err()
            .unwrap();
        assert!(matches!(err, StorageError::PredatesHistory), "{err}");
        assert!(dir.path().join(DB_FILE).exists());
        assert!(
            !deployment_dir(dir.path(), 100, &REG_A)
                .join(DB_FILE)
                .exists()
        );
    }
}

fn post(nonce: u64, fee: u128) -> L1PostRecord {
    L1PostRecord {
        pid: [3; 31],
        root_before: [4; 32],
        nonce,
        tx_hash: alloy::primitives::B256::repeat_byte(nonce as u8),
        fees: L1Fees {
            max_fee: fee,
            tip: 1_000_000,
            blob_fee: 2,
        },
        vhs: vec![alloy::primitives::B256::repeat_byte(1)],
        posted_at: 1_700_000_000,
        from: [5; 20],
    }
}

#[test]
fn l1_posts_round_trip_by_nonce() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    assert!(db.l1_posts().unwrap().is_empty());
    db.put_l1_post(&post(7, 10)).unwrap();
    db.put_l1_post(&post(5, 10)).unwrap();
    // A replacement at the same nonce overwrites its record.
    db.put_l1_post(&post(7, 20)).unwrap();
    let got = db.l1_posts().unwrap();
    assert_eq!(got, vec![post(5, 10), post(7, 20)]);
    // Fees above u64 survive the JSON encoding.
    db.put_l1_post(&post(9, u128::from(u64::MAX) * 4)).unwrap();
    drop(db);
    let db = open(&dir);
    assert_eq!(db.l1_posts().unwrap().len(), 3);
    db.remove_l1_posts_upto(7).unwrap();
    assert_eq!(
        db.l1_posts().unwrap(),
        vec![post(9, u128::from(u64::MAX) * 4)]
    );
}

// Every earlier schema predates the coverage history: no upgrade.
#[test]
fn files_before_the_coverage_history_are_refused() {
    for old in [7, 8, 9, 10] {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = open(&dir);
            db.put_process(&process(3)).unwrap();
            db.set_meta_u64(SCHEMA_VERSION_KEY, old).unwrap();
        }
        let err = Db::open(&dir.path().join("sequencer.redb")).err().unwrap();
        assert!(matches!(err, StorageError::PredatesHistory), "{old}: {err}");
        assert!(
            err.to_string()
                .contains("start this node with a fresh datadir")
        );
    }
}

#[test]
fn dropped_arbo_handle_releases_its_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let store = db.arbo_storage(&pid(1)).unwrap();
    // A node-key read opens the cached snapshot, hit or miss.
    arbo::Storage::get(&store, &[7u8; 32]).unwrap();
    assert_eq!(db.arbo_snapshots_open(), 1);
    drop(store);
    assert_eq!(db.arbo_snapshots_open(), 0);
}

/// The coverage history: an entry per batch root, re-writing a root is
/// idempotent, and `coverage_for` unions exactly the entries naming a vote.
#[test]
fn coverage_history_is_append_by_root_and_read_by_vote() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let rec = |slots: &[u64], vids: &[u64]| ExposedRecord {
        slots: slots.to_vec(),
        vote_ids: vids.to_vec(),
    };
    db.put_coverage(&pid(1), &[1; 32], &rec(&[10, 11], &[100, 101]))
        .unwrap();
    db.put_coverage(&pid(1), &[2; 32], &rec(&[20, 21], &[101]))
        .unwrap();
    db.put_coverage(&pid(1), &[3; 32], &rec(&[30], &[102]))
        .unwrap();
    // Another process's entries never leak in.
    db.put_coverage(&pid(2), &[4; 32], &rec(&[40], &[100]))
        .unwrap();
    let cov = |vids: &[u64]| db.coverage_for(&pid(1), vids).unwrap();
    assert_eq!(cov(&[100]).into_iter().collect::<Vec<_>>(), [10, 11]);
    assert_eq!(
        cov(&[101]).into_iter().collect::<Vec<_>>(),
        [10, 11, 20, 21]
    );
    assert_eq!(
        cov(&[102, 100]).into_iter().collect::<Vec<_>>(),
        [10, 11, 30]
    );
    assert!(cov(&[999]).is_empty());
    // The same root again (a replay) changes nothing.
    db.put_coverage(&pid(1), &[1; 32], &rec(&[10, 11], &[100, 101]))
        .unwrap();
    assert_eq!(db.coverage_history(&pid(1)).unwrap().len(), 3);
    // A large entry (a full batch plus MAX_REFRESH refreshes) round-trips.
    let big: Vec<u64> = (0..2560).collect();
    db.put_coverage(&pid(1), &[5; 32], &rec(&big, &[103]))
        .unwrap();
    drop(db);
    let db = open(&dir);
    assert_eq!(db.coverage_for(&pid(1), &[103]).unwrap().len(), 2560);
}

/// A seal is one commit: the votes go aggregated, the exposed set and the
/// history entry are stored together, or nothing is.
#[test]
fn seal_with_coverage_is_one_commit() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let base = 1u64 << 63;
    let (a, b) = (base + 1, base + 2);
    db.admit_vote(&vote(1, a)).unwrap();
    db.admit_vote(&vote(1, b)).unwrap();
    let exposed = ExposedRecord {
        slots: vec![1, 2, 3],
        vote_ids: vec![a, b],
    };
    let entry = ExposedRecord {
        slots: vec![1, 2, 3],
        vote_ids: vec![a, b],
    };
    db.fail_next_writes(1);
    assert!(
        db.seal_batch_covered(&pid(1), &[a, b], &exposed, &[9; 32], &entry)
            .is_err()
    );
    assert_eq!(
        db.vote(&pid(1), a).unwrap().unwrap().status,
        VoteStatus::Pending
    );
    assert_eq!(db.pending(&pid(1)).unwrap(), vec![a, b]);
    assert_eq!(db.exposed(&pid(1)).unwrap(), None);
    assert!(db.coverage_for(&pid(1), &[a]).unwrap().is_empty());
    db.seal_batch_covered(&pid(1), &[a, b], &exposed, &[9; 32], &entry)
        .unwrap();
    assert_eq!(
        db.vote(&pid(1), a).unwrap().unwrap().status,
        VoteStatus::Aggregated
    );
    assert!(db.pending(&pid(1)).unwrap().is_empty());
    assert_eq!(db.exposed(&pid(1)).unwrap(), Some(exposed));
    assert_eq!(db.coverage_for(&pid(1), &[b]).unwrap().len(), 3);
}
