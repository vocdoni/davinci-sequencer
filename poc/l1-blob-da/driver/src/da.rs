//! Pure, offline logic: SSZ roots, DAVINCI challenge and digest, cell embedding.
//! Every function mirrors `contracts/src/BlobDAPoC.sol`.
use alloy::eips::eip4844::c_kzg::{Blob as KBlob, Bytes32, Bytes48, KzgSettings};
use alloy::eips::eip4844::env_settings::EnvKzgSettings;
use alloy::primitives::U256;
use anyhow::{bail, Context, Result};
use rand::RngCore;
use sha2::{Digest, Sha256};

pub const FIELD_ELEMENTS: usize = 4096;
pub const BLOB_BYTES: usize = FIELD_ELEMENTS * 32;
pub const PAYLOAD_PER_CELL: usize = 31;
pub const MAX_BLOBS: usize = 6;
pub const BEACON_GENESIS: u64 = 1_606_824_023;
pub const SLOT_SECS: u64 = 12;
pub const BLS_MODULUS: U256 = U256::from_limbs([
    0xffffffff00000001,
    0x53bda402fffe5bfe,
    0x3339d80809a1d805,
    0x73eda753299d7d48,
]);

pub fn sha(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

pub fn u64le32(v: u64) -> [u8; 32] {
    let mut o = [0u8; 32];
    o[..8].copy_from_slice(&v.to_le_bytes());
    o
}

/// SSZ hash_tree_root of a BeaconBlockHeader (== the beacon block root).
pub fn header_root(slot: u64, proposer: u64, parent: &[u8; 32], state: &[u8; 32], body: &[u8; 32]) -> [u8; 32] {
    let z = [0u8; 32];
    let n01 = sha(&[&u64le32(slot), &u64le32(proposer)]);
    let n23 = sha(&[parent, state]);
    let n45 = sha(&[body, &z]);
    let n67 = sha(&[&z, &z]);
    sha(&[&sha(&[&n01, &n23]), &sha(&[&n45, &n67])])
}

/// Folds the SSZ branch of body.blob_kzg_commitments[index]; returns the body root it leads to.
pub fn commitment_root(com: &[u8], index: u64, branch: &[[u8; 32]; 17]) -> [u8; 32] {
    assert_eq!(com.len(), 48);
    let mut node = sha(&[&com[..32], &com[32..], &[0u8; 16]]);
    let g = 27u64 * 8192 + index;
    for (i, sib) in branch.iter().enumerate() {
        node = if (g >> i) & 1 == 1 { sha(&[sib, &node]) } else { sha(&[&node, sib]) };
    }
    node
}

/// z = sha256(bytes32(pid) || rootBefore || com) mod BLS_MODULUS (pid right-aligned in 32 bytes).
pub fn challenge_z(pid: &[u8; 31], root_before: &[u8; 32], com: &[u8]) -> [u8; 32] {
    let mut pid32 = [0u8; 32];
    pid32[1..].copy_from_slice(pid);
    let h = U256::from_be_bytes(sha(&[&pid32, root_before, com]));
    (h % BLS_MODULUS).to_be_bytes()
}

/// digest = sha256(com_0 || y_0 || com_1 || y_1 ...).
pub fn blobs_digest(pairs: &[(Vec<u8>, [u8; 32])]) -> [u8; 32] {
    let mut h = Sha256::new();
    for (c, y) in pairs {
        h.update(c);
        h.update(y);
    }
    h.finalize().into()
}

pub fn versioned_hash(com: &[u8]) -> [u8; 32] {
    let mut h = sha(&[com]);
    h[0] = 1;
    h
}

// ------------------------------------------------------------ cells

pub fn n_blobs(len: usize) -> usize {
    len.div_ceil(PAYLOAD_PER_CELL).div_ceil(FIELD_ELEMENTS)
}

/// cell_j = 0x00 || S[31j..31j+31] (last cell zero-padded); 4096 cells per blob.
pub fn embed(s: &[u8]) -> Result<Vec<Vec<u8>>> {
    let n = n_blobs(s.len());
    if n == 0 || n > MAX_BLOBS {
        bail!("stream of {} bytes needs {n} blobs; must be 1..={MAX_BLOBS}", s.len());
    }
    let mut blobs = vec![vec![0u8; BLOB_BYTES]; n];
    for (j, chunk) in s.chunks(PAYLOAD_PER_CELL).enumerate() {
        let (b, c) = (j / FIELD_ELEMENTS, j % FIELD_ELEMENTS);
        blobs[b][c * 32 + 1..c * 32 + 1 + chunk.len()].copy_from_slice(chunk);
    }
    Ok(blobs)
}

pub fn extract(blobs: &[Vec<u8>], len: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(len);
    for b in blobs {
        if b.len() != BLOB_BYTES {
            bail!("blob of {} bytes", b.len());
        }
        for cell in b.chunks(32) {
            if cell[0] != 0 {
                bail!("cell with non-zero leading byte");
            }
            out.extend_from_slice(&cell[1..]);
        }
    }
    if out.len() < len {
        bail!("blobs hold {} < {len} bytes", out.len());
    }
    out.truncate(len);
    Ok(out)
}

// ------------------------------------------------------------ scenarios

pub struct Stream {
    pub name: String,
    pub data: Vec<u8>,
}

pub fn stream_len(name: &str) -> Result<usize> {
    let (n, r, nf) = match name {
        "nf2-steady" => (256, 256, 2),
        "nf16-steady" => (256, 256, 16),
        "max6" => return Ok(761_856),
        _ => bail!("unknown scenario {name:?} (nf2-steady, nf16-steady, max6 with --bytes)"),
    };
    Ok(8 + 8 * n + (n + r) * (8 + 64 * nf))
}

pub fn build_stream(name: &str, bytes: Option<usize>) -> Result<Stream> {
    let len = match bytes {
        Some(b) => b,
        None => stream_len(name)?,
    };
    if len == 0 {
        bail!("empty stream");
    }
    let mut data = vec![0u8; len];
    rand::thread_rng().fill_bytes(&mut data);
    Ok(Stream { name: name.to_string(), data })
}

// ------------------------------------------------------------ kzg

pub fn kzg() -> &'static KzgSettings {
    EnvKzgSettings::Default.get()
}

pub fn commitment(blob: &[u8]) -> Result<[u8; 48]> {
    let b = KBlob::from_bytes(blob).context("blob")?;
    Ok(kzg().blob_to_kzg_commitment(&b).context("commitment")?.to_bytes().into_inner())
}

/// (proof, y) for P(z) with the mainnet trusted setup; the opening is verified before returning.
pub fn open(blob: &[u8], com: &[u8; 48], z: &[u8; 32]) -> Result<([u8; 48], [u8; 32])> {
    let b = KBlob::from_bytes(blob).context("blob")?;
    let zb = Bytes32::from_bytes(z).context("z")?;
    let (proof, y) = kzg().compute_kzg_proof(&b, &zb).context("compute_kzg_proof")?;
    let ok = kzg()
        .verify_kzg_proof(&Bytes48::from_bytes(com)?, &zb, &y, &proof.to_bytes())
        .context("verify_kzg_proof")?;
    if !ok {
        bail!("local KZG opening does not verify");
    }
    Ok((proof.to_bytes().into_inner(), *y.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    pub fn h32(v: &Value) -> [u8; 32] {
        hex::decode(v.as_str().unwrap().trim_start_matches("0x")).unwrap().try_into().unwrap()
    }
    pub fn fixture() -> Value {
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../fixtures/beacon_sidecar.json");
        serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
    }
    fn n(v: &Value) -> u64 {
        v.as_str().unwrap().parse().unwrap()
    }

    #[test]
    fn modulus() {
        assert_eq!(
            BLS_MODULUS.to_string(),
            "52435875175126190479447740508185965837690552500527637822603658699938581184513"
        );
    }

    #[test]
    fn roots_match_fixture() {
        let f = fixture();
        let h = &f["header"];
        let body = h32(&h["body_root"]);
        let root = header_root(n(&h["slot"]), n(&h["proposer_index"]), &h32(&h["parent_root"]), &h32(&h["state_root"]), &body);
        assert_eq!(root, h32(&f["block_root"]));
        let com = hex::decode(f["kzg_commitment"].as_str().unwrap().trim_start_matches("0x")).unwrap();
        let branch: Vec<[u8; 32]> = f["kzg_commitment_inclusion_proof"].as_array().unwrap().iter().map(h32).collect();
        let branch: [[u8; 32]; 17] = branch.try_into().unwrap();
        let idx = f["index"].as_u64().unwrap();
        assert_eq!(commitment_root(&com, idx, &branch), body);
        assert_ne!(commitment_root(&com, idx + 1, &branch), body);
    }

    #[test]
    fn cells_round_trip() {
        for len in [1usize, 30, 31, 32, 71_688, 530_440, 4096 * 31, 4096 * 31 + 1, 761_856] {
            let s = build_stream("x", Some(len)).unwrap().data;
            let blobs = embed(&s).unwrap();
            assert_eq!(blobs.len(), n_blobs(len));
            assert_eq!(extract(&blobs, len).unwrap(), s);
        }
        assert_eq!(n_blobs(71_688), 1);
        assert_eq!(n_blobs(530_440), 5);
        assert_eq!(n_blobs(761_856), 6);
        assert!(embed(&vec![0u8; 761_857]).is_err());
        assert_eq!(stream_len("nf2-steady").unwrap(), 71_688);
        assert_eq!(stream_len("nf16-steady").unwrap(), 530_440);
    }

    /// Cross-checked against contracts/test/BlobDAPoC.t.sol::test_sharedVector.
    #[test]
    fn z_and_digest_vector() {
        let mut pid = [0u8; 31];
        pid[30] = 1;
        let mut rb = [0u8; 32];
        rb[31] = 2;
        let com: Vec<u8> = (0..48u8).collect();
        let z = challenge_z(&pid, &rb, &com);
        let y = [7u8; 32];
        let d = blobs_digest(&[(com, y)]);
        assert_eq!(hex::encode(z), SHARED_Z);
        assert_eq!(hex::encode(d), SHARED_DIGEST);
    }
    const SHARED_Z: &str = "6b583eea0ff2e858075e8d69bd663f0b1d30588f84d4720bd03811c05bdff939";
    const SHARED_DIGEST: &str = "c0767f3eb21214f8e61103382318931dd6a728d70fd5de3179350bf99b59c31f";
}
