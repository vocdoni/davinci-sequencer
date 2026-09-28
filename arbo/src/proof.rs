//! Merkle proofs: generation, PackSiblings/UnpackSiblings wire format and
//! CheckProof, byte-compatible with Go arbo.

use crate::error::Error;
use crate::hash::{EMPTY_HASH, Hash, HashFunction};
use crate::storage::Storage;
use crate::tree::{Tree, check_kv_len, get_path, leaf_node, read_leaf};

/// A (non-)inclusion proof. `key`/`value` are the leaf actually found on the
/// path (empty when the slot is empty); `siblings` run root→leaf.
#[derive(Debug, Clone)]
pub struct Proof {
    pub exists: bool,
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub siblings: Vec<Hash>,
    pub packed: Vec<u8>,
}

impl<S: Storage, H: HashFunction> Tree<S, H> {
    /// Proof of (non-)inclusion of `k` under the current root.
    pub fn gen_proof(&self, k: &[u8]) -> Result<Proof, Error> {
        self.gen_proof_at(&self.root(), k)
    }

    /// Proof of (non-)inclusion of `k` under a historical `root`.
    pub fn gen_proof_at(&self, root: &Hash, k: &[u8]) -> Result<Proof, Error> {
        let path = self.path_of(k)?;
        let dr = self.down_from(root, k, &path, true)?;
        let packed = pack_siblings(&self.hash, &dr.siblings)?;
        let (leaf_k, leaf_v) = read_leaf(&dr.curr_value);
        Ok(Proof {
            exists: leaf_k == k,
            key: leaf_k.to_vec(),
            value: leaf_v.to_vec(),
            siblings: dr.siblings,
            packed,
        })
    }
}

/// Go `PackSiblings`:
/// `[full_len u16 LE | bitmap_len u16 LE | bitmap (LSB-first) | non-zero siblings]`.
/// Errors if the packed size exceeds u16 (only possible above ~500 levels).
pub fn pack_siblings<H: HashFunction>(_h: &H, siblings: &[Hash]) -> Result<Vec<u8>, Error> {
    let mut bitmap = vec![0u8; siblings.len().div_ceil(8)];
    let mut body: Vec<u8> = Vec::new();
    for (i, s) in siblings.iter().enumerate() {
        if *s != EMPTY_HASH {
            bitmap[i / 8] |= 1 << (i % 8);
            body.extend_from_slice(s);
        }
    }
    let full_len = 4 + bitmap.len() + body.len();
    if full_len > u16::MAX as usize {
        return Err(Error::MalformedSiblings("packed length exceeds u16"));
    }
    let mut out = Vec::with_capacity(full_len);
    out.extend_from_slice(&(full_len as u16).to_le_bytes());
    out.extend_from_slice(&(bitmap.len() as u16).to_le_bytes());
    out.extend_from_slice(&bitmap);
    out.extend_from_slice(&body);
    Ok(out)
}

/// Go `UnpackSiblings`, with typed errors where Go would slice-panic.
/// Quirk kept: trailing empty siblings beyond the last non-zero one are
/// dropped (the loop stops when the sibling bytes are exhausted).
pub fn unpack_siblings<H: HashFunction>(_h: &H, b: &[u8]) -> Result<Vec<Hash>, Error> {
    if b.len() < 4 {
        return Err(Error::MalformedSiblings("shorter than header"));
    }
    let full_len = u16::from_le_bytes([b[0], b[1]]) as usize;
    let bitmap_len = u16::from_le_bytes([b[2], b[3]]) as usize;
    if b.len() != full_len {
        return Err(Error::MalformedSiblings("full length mismatch"));
    }
    if 4 + bitmap_len > b.len() {
        return Err(Error::MalformedSiblings("bitmap out of range"));
    }
    let bitmap = &b[4..4 + bitmap_len];
    let body = &b[4 + bitmap_len..];

    let mut siblings: Vec<Hash> = Vec::new();
    let mut off = 0usize;
    for i in 0..bitmap_len * 8 {
        if off >= body.len() {
            break;
        }
        if bitmap[i / 8] & (1 << (i % 8)) != 0 {
            if body.len() - off < 32 {
                return Err(Error::MalformedSiblings("truncated sibling"));
            }
            let mut s = [0u8; 32];
            s.copy_from_slice(&body[off..off + 32]);
            siblings.push(s);
            off += 32;
        } else {
            siblings.push(EMPTY_HASH);
        }
    }
    Ok(siblings)
}

/// Go `CheckProof`: verify a packed (non-)inclusion proof against `root`.
pub fn check_proof<H: HashFunction>(
    h: &H,
    k: &[u8],
    v: &[u8],
    root: &Hash,
    packed_siblings: &[u8],
) -> Result<bool, Error> {
    let siblings = unpack_siblings(h, packed_siblings)?;
    check_kv_len(k, v)?;

    let mut key_path = vec![0u8; siblings.len().div_ceil(8)];
    let n = k.len().min(key_path.len());
    key_path[..n].copy_from_slice(&k[..n]);
    let path = get_path(siblings.len(), &key_path);

    let (mut key, _) = leaf_node(h, k, v);
    for i in (0..siblings.len()).rev() {
        key = if path[i] {
            h.hash(&[&siblings[i], &key])
        } else {
            h.hash(&[&key, &siblings[i]])
        };
    }
    Ok(key == *root)
}
