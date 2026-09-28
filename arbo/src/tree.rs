//! Sparse Merkle tree, a faithful port of vocdoni/arbo's `tree.go`:
//! LSB-first path bits, floating leaves, content-addressed nodes, all
//! historical roots kept.

use std::collections::HashMap;

use crate::error::Error;
use crate::hash::{EMPTY_HASH, Hash, HashFunction};
use crate::storage::{Storage, WriteBatch};

/// Uncommitted nodes layered over storage during an `add_batch`.
pub(crate) type Overlay = HashMap<Vec<u8>, Vec<u8>>;

/// Node type prefixes (Go `PrefixValueEmpty/Leaf/Intermediate`).
pub(crate) const PREFIX_LEAF: u8 = 1;
pub(crate) const PREFIX_INTERMEDIATE: u8 = 2;
/// Go `emptyValue`: the pseudo-value of an empty slot.
pub(crate) const EMPTY_VALUE: [u8; 1] = [0];

pub(crate) const KEY_ROOT: &[u8] = b"root";
pub(crate) const KEY_NLEAFS: &[u8] = b"nleafs";
/// Pinned tree parameters: `[hash_id u8 | max_levels u32 LE]`.
pub(crate) const KEY_PARAMS: &[u8] = b"params";

pub(crate) const MAX_KEY_LEN: usize = u8::MAX as usize;
pub(crate) const MAX_VALUE_LEN: usize = u16::MAX as usize;

/// A key-value of an `add_batch` that could not be added.
#[derive(Debug)]
pub struct Invalid {
    pub index: usize,
    pub error: Error,
}

/// Go `checkKeyValueLen`: dump encoding limits.
pub(crate) fn check_kv_len(k: &[u8], v: &[u8]) -> Result<(), Error> {
    if k.len() > MAX_KEY_LEN {
        return Err(Error::KeyTooLong {
            len: k.len(),
            max: MAX_KEY_LEN,
        });
    }
    if v.len() > MAX_VALUE_LEN {
        return Err(Error::ValueTooLong {
            len: v.len(),
            max: MAX_VALUE_LEN,
        });
    }
    Ok(())
}

/// Go `keyLenByLevels`: ceil(max_levels/8).
pub(crate) fn key_len_by_levels(max_levels: usize) -> usize {
    max_levels.div_ceil(8)
}

/// Go `keyPathFromKey`: zero-pad the key to the tree key length.
pub(crate) fn key_path_from_key(max_levels: usize, k: &[u8]) -> Result<Vec<u8>, Error> {
    let max_key_len = key_len_by_levels(max_levels);
    if k.len() > max_key_len {
        return Err(Error::KeyTooLong {
            len: k.len(),
            max: max_key_len,
        });
    }
    let mut kp = vec![0u8; max_key_len];
    kp[..k.len()].copy_from_slice(k);
    Ok(kp)
}

/// Go `getPath`: LSB-first path bits.
pub(crate) fn get_path(num_levels: usize, key_path: &[u8]) -> Vec<bool> {
    (0..num_levels)
        .map(|n| key_path[n / 8] & (1 << (n % 8)) != 0)
        .collect()
}

/// Go `newLeafValue`: leaf hash `H(k ‖ v ‖ 0x01)` and stored encoding
/// `[0x01, len(k), k, v]`.
pub(crate) fn leaf_node<H: HashFunction>(h: &H, k: &[u8], v: &[u8]) -> (Hash, Vec<u8>) {
    let key = h.hash(&[k, v, &[1]]);
    let mut value = Vec::with_capacity(2 + k.len() + v.len());
    value.push(PREFIX_LEAF);
    value.push(k.len() as u8);
    value.extend_from_slice(k);
    value.extend_from_slice(v);
    (key, value)
}

/// Go `newIntermediate`: node hash `H(l ‖ r)` and encoding `[0x02, 32, l, r]`.
pub(crate) fn intermediate_node<H: HashFunction>(h: &H, l: &Hash, r: &Hash) -> (Hash, Vec<u8>) {
    let key = h.hash(&[l, r]);
    let mut value = Vec::with_capacity(2 + 64);
    value.push(PREFIX_INTERMEDIATE);
    value.push(32);
    value.extend_from_slice(l);
    value.extend_from_slice(r);
    (key, value)
}

/// Go `ReadLeafValue` (lenient: short encodings read as empty).
pub(crate) fn read_leaf(b: &[u8]) -> (&[u8], &[u8]) {
    if b.len() < 2 {
        return (&[], &[]);
    }
    let klen = b[1] as usize;
    if b.len() < 2 + klen {
        return (&[], &[]);
    }
    (&b[2..2 + klen], &b[2 + klen..])
}

/// Read the two children of a length-checked intermediate node.
fn read_intermediate(b: &[u8]) -> (Hash, Hash) {
    let mut l = [0u8; 32];
    let mut r = [0u8; 32];
    l.copy_from_slice(&b[2..34]);
    r.copy_from_slice(&b[34..66]);
    (l, r)
}

/// Result of walking down to a leaf slot.
pub(crate) struct DownResult {
    /// Stored node value ([0] for an empty slot).
    pub curr_value: Vec<u8>,
    /// Siblings collected root→leaf.
    pub siblings: Vec<Hash>,
}

/// Sparse Merkle tree over a `Storage`, arbo-compatible byte for byte.
pub struct Tree<S: Storage, H: HashFunction> {
    pub(crate) storage: S,
    pub(crate) max_levels: usize,
    pub(crate) hash: H,
    root: Hash,
}

impl<S: Storage, H: HashFunction> Tree<S, H> {
    /// Open a tree on `storage`, resuming from a stored root if present.
    /// Parameters are pinned in storage; reopening with a different
    /// `max_levels` or hash function fails with `ParamsMismatch`.
    pub fn new(storage: S, max_levels: usize, hash: H) -> Result<Self, Error> {
        let levels = u32::try_from(max_levels).map_err(|_| Error::ParamsMismatch("max_levels"))?;
        let mut params = vec![hash.id()];
        params.extend_from_slice(&levels.to_le_bytes());
        match storage.get(KEY_PARAMS)? {
            Some(stored) if stored != params => {
                return Err(if stored.first() != params.first() {
                    Error::ParamsMismatch("hash function")
                } else {
                    Error::ParamsMismatch("max_levels")
                });
            }
            Some(_) => {}
            // Params are written only when the storage is truly empty; a
            // tree with nodes but no params is from an unknown writer.
            None if storage.get(KEY_ROOT)?.is_some() => {
                return Err(Error::ParamsMismatch("params missing"));
            }
            None => {
                let mut batch = WriteBatch::new();
                batch.put(KEY_PARAMS.to_vec(), params);
                storage.write(batch)?;
            }
        }
        let root = match storage.get(KEY_ROOT)? {
            Some(b) => b
                .try_into()
                .map_err(|_| Error::Corrupted("stored root length"))?,
            None => EMPTY_HASH,
        };
        Ok(Self {
            storage,
            max_levels,
            hash,
            root,
        })
    }

    /// The current root.
    pub fn root(&self) -> Hash {
        self.root
    }

    pub fn max_levels(&self) -> usize {
        self.max_levels
    }

    /// Number of leafs added (updates don't count).
    pub fn n_leafs(&self) -> Result<u64, Error> {
        match self.storage.get(KEY_NLEAFS)? {
            Some(b) => Ok(u64::from_le_bytes(
                b.as_slice()
                    .try_into()
                    .map_err(|_| Error::Corrupted("nleafs length"))?,
            )),
            None => Ok(0),
        }
    }

    /// Rewind (or fast-forward) to a root already present in storage.
    pub fn set_root(&mut self, root: &Hash) -> Result<(), Error> {
        if *root != EMPTY_HASH && self.storage.get(root)?.is_none() {
            return Err(Error::RootNotFound);
        }
        let mut batch = WriteBatch::new();
        batch.put(KEY_ROOT.to_vec(), root.to_vec());
        self.storage.write(batch)?;
        self.root = *root;
        Ok(())
    }

    pub(crate) fn path_of(&self, k: &[u8]) -> Result<Vec<bool>, Error> {
        let kp = key_path_from_key(self.max_levels, k)?;
        Ok(get_path(self.max_levels, &kp))
    }

    /// Go `down`: walk from `root` towards the slot of `new_key`, collecting
    /// siblings. With `get_leaf` it stops at the first leaf found; without
    /// it, it extends virtually below an occupying leaf.
    pub(crate) fn down_from(
        &self,
        root: &Hash,
        new_key: &[u8],
        path: &[bool],
        get_leaf: bool,
    ) -> Result<DownResult, Error> {
        self.down_with(None, root, new_key, path, get_leaf)
    }

    /// `down_from` reading through an optional uncommitted overlay.
    pub(crate) fn down_with(
        &self,
        overlay: Option<&Overlay>,
        root: &Hash,
        new_key: &[u8],
        path: &[bool],
        get_leaf: bool,
    ) -> Result<DownResult, Error> {
        let mut curr = *root;
        let mut siblings: Vec<Hash> = Vec::new();
        let mut lvl = 0usize;
        loop {
            if lvl > self.max_levels {
                return Err(Error::MaxLevel);
            }
            if curr == EMPTY_HASH {
                return Ok(DownResult {
                    curr_value: EMPTY_VALUE.to_vec(),
                    siblings,
                });
            }
            let value = self
                .get_node(overlay, &curr)?
                .ok_or(Error::Corrupted("missing node"))?;
            match value.first().copied() {
                Some(PREFIX_LEAF) => {
                    if !get_leaf {
                        let (leaf_k, _) = read_leaf(&value);
                        if leaf_k == new_key {
                            return Err(Error::KeyAlreadyExists);
                        }
                        // A stored leaf key that no longer fits the path
                        // is storage corruption, not a caller error.
                        let old_path = self
                            .path_of(leaf_k)
                            .map_err(|_| Error::Corrupted("stored leaf key too long"))?;
                        down_virtually(
                            &mut siblings,
                            &curr,
                            &old_path,
                            path,
                            lvl,
                            self.max_levels,
                        )?;
                    }
                    return Ok(DownResult {
                        curr_value: value,
                        siblings,
                    });
                }
                Some(PREFIX_INTERMEDIATE) => {
                    if value.len() != 2 + 64 {
                        return Err(Error::Corrupted("intermediate node length"));
                    }
                    // A crafted/corrupted chain (or a tree opened with a
                    // smaller max_levels) could run past the path bits.
                    if lvl >= path.len() {
                        return Err(Error::Corrupted("intermediate below max_levels"));
                    }
                    let (l, r) = read_intermediate(&value);
                    if path[lvl] {
                        siblings.push(l);
                        curr = r;
                    } else {
                        siblings.push(r);
                        curr = l;
                    }
                    lvl += 1;
                }
                // Go panics here ("should not be reached"); fail typed.
                Some(0) => return Err(Error::Corrupted("empty-prefix node")),
                Some(_) => return Err(Error::InvalidValuePrefix),
                None => return Err(Error::Corrupted("empty node value")),
            }
        }
    }

    /// Node lookup through an optional uncommitted overlay.
    fn get_node(&self, overlay: Option<&Overlay>, key: &Hash) -> Result<Option<Vec<u8>>, Error> {
        if let Some(o) = overlay
            && let Some(v) = o.get(key.as_slice())
        {
            return Ok(Some(v.clone()));
        }
        self.storage.get(key)
    }

    /// Go `up`: fold the collected siblings back to a new root, emitting the
    /// intermediate nodes through `put`.
    fn up(
        &self,
        put: &mut dyn FnMut(Hash, Vec<u8>),
        leaf_key: Hash,
        siblings: &[Hash],
        path: &[bool],
    ) -> Result<Hash, Error> {
        if siblings.len() > path.len() {
            return Err(Error::Corrupted("more siblings than path bits"));
        }
        let mut key = leaf_key;
        for lvl in (0..siblings.len()).rev() {
            let (l, r) = if path[lvl] {
                (&siblings[lvl], &key)
            } else {
                (&key, &siblings[lvl])
            };
            let (k, v) = intermediate_node(&self.hash, l, r);
            put(k, v);
            key = k;
        }
        Ok(key)
    }

    /// Insert a new key. Fails with `KeyAlreadyExists` if present.
    pub fn add(&mut self, k: &[u8], v: &[u8]) -> Result<(), Error> {
        check_kv_len(k, v)?;
        let path = self.path_of(k)?;
        let dr = self.down_from(&self.root, k, &path, false)?;

        let (leaf_key, leaf_value) = leaf_node(&self.hash, k, v);
        let mut batch = WriteBatch::new();
        batch.put(leaf_key.to_vec(), leaf_value);
        let new_root = if dr.siblings.is_empty() {
            leaf_key
        } else {
            self.up(
                &mut |k, v| batch.put(k.to_vec(), v),
                leaf_key,
                &dr.siblings,
                &path,
            )?
        };

        let n_leafs = self.n_leafs()? + 1;
        batch.put(KEY_ROOT.to_vec(), new_root.to_vec());
        batch.put(KEY_NLEAFS.to_vec(), n_leafs.to_le_bytes().to_vec());
        self.storage.write(batch)?;
        self.root = new_root;
        Ok(())
    }

    /// Update the value of an existing key.
    pub fn update(&mut self, k: &[u8], v: &[u8]) -> Result<(), Error> {
        check_kv_len(k, v)?;
        let path = self.path_of(k)?;
        let dr = self.down_from(&self.root, k, &path, true)?;
        let (leaf_k, _) = read_leaf(&dr.curr_value);
        if leaf_k != k {
            return Err(Error::KeyNotFound);
        }

        let (leaf_key, leaf_value) = leaf_node(&self.hash, k, v);
        let mut batch = WriteBatch::new();
        batch.put(leaf_key.to_vec(), leaf_value);
        let new_root = if dr.siblings.is_empty() {
            leaf_key
        } else {
            self.up(
                &mut |k, v| batch.put(k.to_vec(), v),
                leaf_key,
                &dr.siblings,
                &path,
            )?
        };
        batch.put(KEY_ROOT.to_vec(), new_root.to_vec());
        self.storage.write(batch)?;
        self.root = new_root;
        Ok(())
    }

    /// `add` against an uncommitted overlay: reads see `overlay` over
    /// storage, writes land in `overlay`, and `root` is advanced in place.
    /// Nothing touches storage; the caller commits the overlay atomically.
    pub(crate) fn add_with_overlay(
        &self,
        overlay: &mut Overlay,
        root: &mut Hash,
        k: &[u8],
        v: &[u8],
    ) -> Result<(), Error> {
        check_kv_len(k, v)?;
        let path = self.path_of(k)?;
        let dr = self.down_with(Some(overlay), root, k, &path, false)?;

        let (leaf_key, leaf_value) = leaf_node(&self.hash, k, v);
        let new_root = if dr.siblings.is_empty() {
            leaf_key
        } else {
            let mut pending: Vec<(Hash, Vec<u8>)> = Vec::with_capacity(dr.siblings.len());
            let nr = self.up(
                &mut |k, v| pending.push((k, v)),
                leaf_key,
                &dr.siblings,
                &path,
            )?;
            for (k, v) in pending {
                overlay.insert(k.to_vec(), v);
            }
            nr
        };
        overlay.insert(leaf_key.to_vec(), leaf_value);
        *root = new_root;
        Ok(())
    }

    /// Value stored under `k`, or None if absent.
    pub fn get(&self, k: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        let path = self.path_of(k)?;
        let dr = self.down_from(&self.root, k, &path, true)?;
        let (leaf_k, leaf_v) = read_leaf(&dr.curr_value);
        if leaf_k == k {
            Ok(Some(leaf_v.to_vec()))
        } else {
            Ok(None)
        }
    }

    /// DFS-preorder iteration over every node under `root` (Go `Iterate`):
    /// intermediates, leaves and empty children (key EMPTY, value 32 zeros).
    pub fn iterate(&self, root: &Hash, mut f: impl FnMut(&[u8], &[u8])) -> Result<(), Error> {
        self.iter_node(root, 0, &mut f)
    }

    fn iter_node(
        &self,
        k: &Hash,
        depth: usize,
        f: &mut dyn FnMut(&[u8], &[u8]),
    ) -> Result<(), Error> {
        // Depth cap: corrupted storage must not loop forever.
        if depth > self.max_levels {
            return Err(Error::Corrupted("tree deeper than max_levels"));
        }
        if *k == EMPTY_HASH {
            f(k, &EMPTY_HASH);
            return Ok(());
        }
        let v = self
            .storage
            .get(k)?
            .ok_or(Error::Corrupted("missing node"))?;
        match v.first().copied() {
            Some(PREFIX_LEAF) | Some(0) => {
                f(k, &v);
                Ok(())
            }
            Some(PREFIX_INTERMEDIATE) => {
                if v.len() != 2 + 64 {
                    return Err(Error::Corrupted("intermediate node length"));
                }
                f(k, &v);
                let (l, r) = read_intermediate(&v);
                self.iter_node(&l, depth + 1, f)?;
                self.iter_node(&r, depth + 1, f)
            }
            Some(_) => Err(Error::InvalidValuePrefix),
            None => Err(Error::Corrupted("empty node value")),
        }
    }

    pub(crate) fn write_batch_root(
        &mut self,
        batch: WriteBatch,
        new_root: Option<Hash>,
    ) -> Result<(), Error> {
        self.storage.write(batch)?;
        if let Some(r) = new_root {
            self.root = r;
        }
        Ok(())
    }
}

/// Go `downVirtually`: extend the path below an occupying leaf until the two
/// keys diverge, pushing EMPTY siblings and finally the old leaf.
fn down_virtually(
    siblings: &mut Vec<Hash>,
    old_leaf_key: &Hash,
    old_path: &[bool],
    new_path: &[bool],
    mut lvl: usize,
    max_levels: usize,
) -> Result<(), Error> {
    loop {
        if lvl + 1 > max_levels {
            return Err(Error::MaxVirtualLevel);
        }
        if old_path[lvl] == new_path[lvl] {
            siblings.push(EMPTY_HASH);
            lvl += 1;
        } else {
            siblings.push(*old_leaf_key);
            return Ok(());
        }
    }
}
