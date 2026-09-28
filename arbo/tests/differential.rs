//! Differential tests against Go arbo: replay the vectors written by
//! `testdata/gen/main.go` and require byte-for-byte equality of roots,
//! values, packed siblings, circom proofs and dumps, on both storage
//! backends.

use arbo::{MemoryStorage, Sha256, Storage, Tree, check_proof};
use serde::Deserialize;
use sha2::Digest;

#[derive(Deserialize)]
struct CircomRec {
    root: String,
    siblings: Vec<String>,
    old_key: String,
    old_value: String,
    is_old0: bool,
    key: String,
    value: String,
    fnc: i32,
}

#[derive(Deserialize)]
struct OpRec {
    op: String,
    key: String,
    #[serde(default)]
    value: String,
    #[serde(default)]
    err: bool,
    #[serde(default)]
    root: String,
    #[serde(default)]
    found: bool,
    #[serde(default)]
    leaf_key: String,
    #[serde(default)]
    leaf_value: String,
    #[serde(default)]
    packed: String,
    #[serde(default)]
    existence: bool,
    #[serde(default)]
    check: bool,
    #[serde(default)]
    circom: Option<CircomRec>,
}

#[derive(Deserialize)]
struct VectorFile {
    max_levels: usize,
    seed: u32,
    ops: Vec<OpRec>,
    final_root: String,
    n_leafs: u64,
    dump: String,
    batch_n: usize,
    batch_root: String,
    batch_invalid: usize,
}

fn hx(b: &[u8]) -> String {
    hex::encode(b)
}

fn unhex(s: &str) -> Vec<u8> {
    hex::decode(s).expect("bad hex in vector")
}

/// Go records a padded circom sibling as the 1-byte `emptyValue` ("00");
/// the Rust API uses 32-byte hashes everywhere.
fn sib32(s: &str) -> [u8; 32] {
    let b = unhex(s);
    let mut out = [0u8; 32];
    out[..b.len()].copy_from_slice(&b);
    out
}

/// The deterministic 10k-batch key-values, identical to the Go generator.
fn batch_kvs(levels: usize, seed: u32, n: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    let key_len = levels.div_ceil(8);
    (0..n)
        .map(|i| {
            let mut input = b"arbo-batch".to_vec();
            input.extend_from_slice(&(levels as u32).to_le_bytes());
            input.extend_from_slice(&seed.to_le_bytes());
            input.extend_from_slice(&(i as u32).to_le_bytes());
            let h: [u8; 32] = sha2::Sha256::digest(&input).into();
            let v: [u8; 32] = sha2::Sha256::digest(h).into();
            (h[..key_len].to_vec(), v.to_vec())
        })
        .collect()
}

fn load(name: &str) -> VectorFile {
    let path = format!("{}/testdata/{}", env!("CARGO_MANIFEST_DIR"), name);
    let data = std::fs::read_to_string(&path).expect("vector file missing; run testdata/gen");
    serde_json::from_str(&data).expect("bad vector json")
}

fn replay<S: Storage>(vf: &VectorFile, mk: &mut dyn FnMut() -> S) {
    let ctx = format!("levels={} seed={}", vf.max_levels, vf.seed);
    let mut tree = Tree::new(mk(), vf.max_levels, Sha256).unwrap();

    for (i, op) in vf.ops.iter().enumerate() {
        let key = unhex(&op.key);
        match op.op.as_str() {
            "add" => {
                let res = tree.add(&key, &unhex(&op.value));
                assert_eq!(res.is_err(), op.err, "{ctx} op {i} add err: {res:?}");
                assert_eq!(hx(&tree.root()), op.root, "{ctx} op {i} add root");
            }
            "update" => {
                let res = tree.update(&key, &unhex(&op.value));
                assert_eq!(res.is_err(), op.err, "{ctx} op {i} update err: {res:?}");
                assert_eq!(hx(&tree.root()), op.root, "{ctx} op {i} update root");
            }
            "get" => {
                let got = tree.get(&key).unwrap();
                assert_eq!(got.is_some(), op.found, "{ctx} op {i} get found");
                if let Some(v) = got {
                    assert_eq!(hx(&v), op.leaf_value, "{ctx} op {i} get value");
                }
            }
            "gen_proof" => {
                let p = tree.gen_proof(&key).unwrap();
                assert_eq!(p.exists, op.existence, "{ctx} op {i} existence");
                assert_eq!(hx(&p.key), op.leaf_key, "{ctx} op {i} leaf key");
                assert_eq!(hx(&p.value), op.leaf_value, "{ctx} op {i} leaf value");
                assert_eq!(hx(&p.packed), op.packed, "{ctx} op {i} packed");
                // CheckProof over the leaf actually found, as the generator does.
                let (ck, cv): (&[u8], &[u8]) = if p.exists {
                    (&key, &p.value)
                } else {
                    (&p.key, &p.value)
                };
                if p.exists || !p.key.is_empty() {
                    let ok = check_proof(&Sha256, ck, cv, &tree.root(), &p.packed).unwrap();
                    assert_eq!(ok, op.check, "{ctx} op {i} check_proof");
                }

                let c = op.circom.as_ref().expect("gen_proof op without circom rec");
                let cp = tree.circom_verifier_proof(&key).unwrap();
                assert_eq!(hx(&cp.root), c.root, "{ctx} op {i} circom root");
                assert_eq!(
                    cp.siblings.len(),
                    c.siblings.len(),
                    "{ctx} op {i} circom siblings len"
                );
                for (j, s) in c.siblings.iter().enumerate() {
                    assert_eq!(cp.siblings[j], sib32(s), "{ctx} op {i} circom sibling {j}");
                }
                assert_eq!(hx(&cp.old_key), c.old_key, "{ctx} op {i} circom old_key");
                assert_eq!(
                    hx(&cp.old_value),
                    c.old_value,
                    "{ctx} op {i} circom old_value"
                );
                assert_eq!(cp.is_old0, c.is_old0, "{ctx} op {i} circom is_old0");
                assert_eq!(hx(&cp.key), c.key, "{ctx} op {i} circom key");
                assert_eq!(hx(&cp.value), c.value, "{ctx} op {i} circom value");
                assert_eq!(i32::from(cp.fnc), c.fnc, "{ctx} op {i} circom fnc");
            }
            other => panic!("unknown op {other}"),
        }
    }

    assert_eq!(hx(&tree.root()), vf.final_root, "{ctx} final root");
    assert_eq!(tree.n_leafs().unwrap(), vf.n_leafs, "{ctx} n_leafs");

    // Dump must match Go's byte for byte; importing it must rebuild the root.
    let dump = tree.dump(&tree.root()).unwrap();
    assert_eq!(hx(&dump), vf.dump, "{ctx} dump");
    let mut imported = Tree::new(mk(), vf.max_levels, Sha256).unwrap();
    imported.import_dump(&dump).unwrap();
    assert_eq!(
        hx(&imported.root()),
        vf.final_root,
        "{ctx} import dump root"
    );

    // 10k-leaf AddBatch on a fresh tree.
    let kvs = batch_kvs(vf.max_levels, vf.seed, vf.batch_n);
    let mut bt = Tree::new(mk(), vf.max_levels, Sha256).unwrap();
    let invalids = bt.add_batch(&kvs).unwrap();
    assert_eq!(invalids.len(), vf.batch_invalid, "{ctx} batch invalids");
    assert_eq!(hx(&bt.root()), vf.batch_root, "{ctx} batch root");
}

const FILES: [&str; 9] = [
    "sha256_64_1.json",
    "sha256_64_2.json",
    "sha256_64_3.json",
    "sha256_160_1.json",
    "sha256_160_2.json",
    "sha256_160_3.json",
    "sha256_256_1.json",
    "sha256_256_2.json",
    "sha256_256_3.json",
];

#[test]
fn differential_memory() {
    for f in FILES {
        let vf = load(f);
        replay(&vf, &mut MemoryStorage::new);
    }
}

#[cfg(feature = "redb")]
#[test]
fn differential_redb() {
    use std::sync::Arc;

    for f in FILES {
        let vf = load(f);
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(redb::Database::create(dir.path().join("t.redb")).unwrap());
        let mut n = 0usize;
        let mut mk = move || {
            n += 1;
            let mut s = arbo::RedbStorage::new(db.clone(), &format!("tree{n}")).unwrap();
            s.set_durability(redb::Durability::None);
            s
        };
        replay(&vf, &mut mk);
    }
}

#[derive(serde::Deserialize)]
struct BatchDiskVector {
    max_levels: usize,
    base_seed: u32,
    base_n: usize,
    batch_seed: u32,
    batch_n: usize,
    base_root: String,
    final_root: String,
    base_invalid: usize,
    batch_invalid: usize,
}

/// Go's `addBatchInDisk` (AddBatch into a populated pebble tree above the
/// 65536-leaf threshold) vs the Rust incremental `add_batch`, plus a >50k
/// `import_dump` whose second chunk goes through the incremental path.
fn replay_batch_disk<S: Storage>(mk: &mut dyn FnMut() -> S) {
    let path = format!(
        "{}/testdata/sha256_batch_disk.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let data = std::fs::read_to_string(path).expect("vector file missing; run testdata/gen");
    let vf: BatchDiskVector = serde_json::from_str(&data).expect("bad vector json");

    let base = batch_kvs(vf.max_levels, vf.base_seed, vf.base_n);
    let mut t = Tree::new(mk(), vf.max_levels, Sha256).unwrap();
    let inv = t.add_batch(&base).unwrap();
    assert_eq!(inv.len(), vf.base_invalid, "base invalids");
    assert_eq!(hx(&t.root()), vf.base_root, "base root");

    // 70k-leaf dump into a fresh tree: chunk 1 (50k) takes the bulk path,
    // chunk 2 (20k) the incremental one.
    let dump = t.dump(&t.root()).unwrap();
    let mut imp = Tree::new(mk(), vf.max_levels, Sha256).unwrap();
    imp.import_dump(&dump).unwrap();
    assert_eq!(hx(&imp.root()), vf.base_root, "import dump root");
    assert_eq!(imp.n_leafs().unwrap(), vf.base_n as u64);

    let batch = batch_kvs(vf.max_levels, vf.batch_seed, vf.batch_n);
    let inv = t.add_batch(&batch).unwrap();
    assert_eq!(inv.len(), vf.batch_invalid, "batch invalids");
    assert_eq!(hx(&t.root()), vf.final_root, "final root");
}

#[test]
fn differential_batch_disk_memory() {
    replay_batch_disk(&mut MemoryStorage::new);
}

#[cfg(feature = "redb")]
#[test]
fn differential_batch_disk_redb() {
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(redb::Database::create(dir.path().join("t.redb")).unwrap());
    let mut n = 0usize;
    let mut mk = move || {
        n += 1;
        let mut s = arbo::RedbStorage::new(db.clone(), &format!("disk{n}")).unwrap();
        s.set_durability(redb::Durability::None);
        s
    };
    replay_batch_disk(&mut mk);
}
