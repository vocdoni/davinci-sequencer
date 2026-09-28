//! Virtual tree (Go `vt.go`): place all leaves without hashing, then compute
//! every node hash exactly once, parallelized with rayon near the top.
//! Backs `Tree::add_batch` and `import_dump`.

use crate::error::Error;
use crate::hash::{EMPTY_HASH, Hash, HashFunction};
use crate::storage::{Storage, WriteBatch};
use crate::tree::{
    Invalid, KEY_NLEAFS, KEY_ROOT, Overlay, Tree, check_kv_len, get_path, intermediate_node,
    key_path_from_key, leaf_node,
};

/// (node key, node value) pairs destined for storage.
type NodePairs = Vec<(Vec<u8>, Vec<u8>)>;

enum VNode {
    Leaf {
        k: Vec<u8>,
        v: Vec<u8>,
        path: Vec<bool>,
    },
    Mid {
        l: Option<Box<VNode>>,
        r: Option<Box<VNode>>,
    },
}

struct VTree {
    root: Option<Box<VNode>>,
    max_levels: usize,
}

impl VTree {
    fn new(max_levels: usize) -> Self {
        Self {
            root: None,
            max_levels,
        }
    }

    /// Go `vt.add` (sequential placement).
    fn add(&mut self, k: &[u8], v: &[u8]) -> Result<(), Error> {
        check_kv_len(k, v)?;
        let kp = key_path_from_key(self.max_levels, k)?;
        let path = get_path(self.max_levels, &kp);
        let leaf = VNode::Leaf {
            k: k.to_vec(),
            v: v.to_vec(),
            path,
        };
        match &mut self.root {
            None => {
                self.root = Some(Box::new(leaf));
                Ok(())
            }
            Some(root) => vnode_add(root, 0, self.max_levels, leaf),
        }
    }

    /// Hash every node; returns the root hash and the (node key, node value)
    /// pairs to store. `par_depth` levels near the root fork rayon tasks.
    fn compute_hashes<H: HashFunction>(&self, h: &H, par_depth: usize) -> (Hash, NodePairs) {
        match &self.root {
            None => (EMPTY_HASH, Vec::new()),
            Some(n) => {
                let mut out = Vec::new();
                let root = node_hashes(n, 0, par_depth, h, &mut out);
                (root, out)
            }
        }
    }
}

fn leaf_bit(leaf: &VNode, lvl: usize) -> bool {
    match leaf {
        VNode::Leaf { path, .. } => path[lvl],
        VNode::Mid { .. } => unreachable!("leaf_bit on mid node"),
    }
}

/// Go `node.add` + `downUntilDivergence`. Unlike Go, a divergence past
/// `max_levels` is detected before mutating, so a failed add never corrupts
/// the virtual tree.
fn vnode_add(n: &mut VNode, lvl: usize, max_levels: usize, leaf: VNode) -> Result<(), Error> {
    if lvl + 1 > max_levels {
        return Err(Error::MaxVirtualLevel);
    }
    match n {
        VNode::Mid { l, r } => {
            let slot = if leaf_bit(&leaf, lvl) { r } else { l };
            match slot {
                None => {
                    *slot = Some(Box::new(leaf));
                    Ok(())
                }
                Some(child) => vnode_add(child, lvl + 1, max_levels, leaf),
            }
        }
        VNode::Leaf { k, path, .. } => {
            let (new_k, new_path) = match &leaf {
                VNode::Leaf { k, path, .. } => (k, path),
                VNode::Mid { .. } => unreachable!(),
            };
            if k == new_k {
                return Err(Error::KeyAlreadyExists);
            }
            // First divergence level at or after lvl.
            let mut d = lvl;
            while d < max_levels && path[d] == new_path[d] {
                d += 1;
            }
            if d >= max_levels {
                return Err(Error::MaxVirtualLevel);
            }
            let bits = new_path[lvl..=d].to_vec();
            let old_leaf = std::mem::replace(n, VNode::Mid { l: None, r: None });
            // Chain of one-child mids down to the divergence level.
            let mut cur: &mut VNode = n;
            for bit in &bits[..d - lvl] {
                let VNode::Mid { l, r } = cur else {
                    unreachable!()
                };
                let slot = if *bit { r } else { l };
                *slot = Some(Box::new(VNode::Mid { l: None, r: None }));
                cur = slot.as_mut().expect("just inserted");
            }
            let VNode::Mid { l, r } = cur else {
                unreachable!()
            };
            if bits[d - lvl] {
                *l = Some(Box::new(old_leaf));
                *r = Some(Box::new(leaf));
            } else {
                *l = Some(Box::new(leaf));
                *r = Some(Box::new(old_leaf));
            }
            Ok(())
        }
    }
}

fn node_hashes<H: HashFunction>(
    n: &VNode,
    depth: usize,
    par_depth: usize,
    h: &H,
    out: &mut NodePairs,
) -> Hash {
    match n {
        VNode::Leaf { k, v, .. } => {
            let (lk, lv) = leaf_node(h, k, v);
            out.push((lk.to_vec(), lv));
            lk
        }
        VNode::Mid { l, r } => {
            let (lh, rh) = if depth < par_depth {
                let ((lh, mut lo), (rh, mut ro)) = rayon::join(
                    || subtree_hashes(l, depth + 1, par_depth, h),
                    || subtree_hashes(r, depth + 1, par_depth, h),
                );
                out.append(&mut lo);
                out.append(&mut ro);
                (lh, rh)
            } else {
                (
                    child_hash(l, depth + 1, par_depth, h, out),
                    child_hash(r, depth + 1, par_depth, h, out),
                )
            };
            let (k, v) = intermediate_node(h, &lh, &rh);
            out.push((k.to_vec(), v));
            k
        }
    }
}

fn child_hash<H: HashFunction>(
    c: &Option<Box<VNode>>,
    depth: usize,
    par_depth: usize,
    h: &H,
    out: &mut NodePairs,
) -> Hash {
    match c {
        None => EMPTY_HASH,
        Some(n) => node_hashes(n, depth, par_depth, h, out),
    }
}

fn subtree_hashes<H: HashFunction>(
    c: &Option<Box<VNode>>,
    depth: usize,
    par_depth: usize,
    h: &H,
) -> (Hash, NodePairs) {
    let mut out = Vec::new();
    let hash = child_hash(c, depth, par_depth, h, &mut out);
    (hash, out)
}

impl<S: Storage, H: HashFunction> Tree<S, H> {
    /// Go `AddBatch`: bulk virtual-tree hashing for an empty tree, and an
    /// incremental path for a populated one that only visits (and re-hashes)
    /// the paths the new keys touch. Both commit nodes + root + nleafs
    /// atomically and return the key-values that could not be added.
    pub fn add_batch(&mut self, kvs: &[(Vec<u8>, Vec<u8>)]) -> Result<Vec<Invalid>, Error> {
        if self.root() == EMPTY_HASH {
            self.add_batch_bulk(kvs)
        } else {
            self.add_batch_incremental(kvs)
        }
    }

    /// Sequential adds through an in-memory overlay, one commit at the end.
    /// Root-equivalent to the virtual tree by history independence.
    fn add_batch_incremental(&mut self, kvs: &[(Vec<u8>, Vec<u8>)]) -> Result<Vec<Invalid>, Error> {
        let mut overlay = Overlay::new();
        let mut root = self.root();
        let mut invalids = Vec::new();
        for (i, (k, v)) in kvs.iter().enumerate() {
            match self.add_with_overlay(&mut overlay, &mut root, k, v) {
                Ok(()) => {}
                // Only errors caused by the caller's own key/value are
                // per-key failures; anything else (broken storage, a
                // corrupted stored node or chain) aborts with nothing
                // written.
                Err(
                    e @ (Error::KeyAlreadyExists
                    | Error::MaxVirtualLevel
                    | Error::KeyTooLong { .. }
                    | Error::ValueTooLong { .. }),
                ) => invalids.push(Invalid { index: i, error: e }),
                Err(e) => return Err(e),
            }
        }

        let n_leafs = self.n_leafs()? + (kvs.len() - invalids.len()) as u64;
        let mut batch = WriteBatch::new();
        for (k, v) in overlay {
            batch.put(k, v);
        }
        batch.put(KEY_ROOT.to_vec(), root.to_vec());
        batch.put(KEY_NLEAFS.to_vec(), n_leafs.to_le_bytes().to_vec());
        self.write_batch_root(batch, Some(root))?;
        Ok(invalids)
    }

    /// Bulk path for an empty tree: place everything in the virtual tree
    /// and hash each node exactly once.
    fn add_batch_bulk(&mut self, kvs: &[(Vec<u8>, Vec<u8>)]) -> Result<Vec<Invalid>, Error> {
        let mut vt = VTree::new(self.max_levels());
        let mut invalids = Vec::new();
        for (i, (k, v)) in kvs.iter().enumerate() {
            if let Err(e) = vt.add(k, v) {
                invalids.push(Invalid { index: i, error: e });
            }
        }

        // Parallelize only when the batch is worth the fork overhead.
        let par_depth = if kvs.len() >= 1024 {
            rayon::current_num_threads().max(1).ilog2() as usize
        } else {
            0
        };
        let has_root = vt.root.is_some();
        let (new_root, pairs) = vt.compute_hashes(&self.hash, par_depth);

        let mut batch = WriteBatch::new();
        for (k, v) in pairs {
            batch.put(k, v);
        }
        if has_root {
            batch.put(KEY_ROOT.to_vec(), new_root.to_vec());
        }
        let n_leafs = self.n_leafs()? + (kvs.len() - invalids.len()) as u64;
        batch.put(KEY_NLEAFS.to_vec(), n_leafs.to_le_bytes().to_vec());
        self.write_batch_root(batch, has_root.then_some(new_root))?;
        Ok(invalids)
    }
}
