//! Tree dump/import, byte-compatible with Go arbo:
//! per leaf `[len(k) u8 | len(v) u16 LE | k | v]`, DFS-preorder order.

use crate::error::Error;
use crate::hash::{EMPTY_HASH, Hash, HashFunction};
use crate::storage::Storage;
use crate::tree::{PREFIX_LEAF, Tree, read_leaf};

/// Go `importDumpKeysPerBatch`.
const IMPORT_KEYS_PER_BATCH: usize = 50_000;

impl<S: Storage, H: HashFunction> Tree<S, H> {
    /// Export all leaves under `root` (Go `Dump`).
    pub fn dump(&self, root: &Hash) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        let mut oversized = false;
        self.iterate(root, |_k, v| {
            if v.first() != Some(&PREFIX_LEAF) {
                return;
            }
            let (lk, lv) = read_leaf(v);
            // Bounded at insertion, but storage bytes are untrusted.
            if lk.len() > u8::MAX as usize || lv.len() > u16::MAX as usize {
                oversized = true;
                return;
            }
            out.push(lk.len() as u8);
            out.extend_from_slice(&(lv.len() as u16).to_le_bytes());
            out.extend_from_slice(lk);
            out.extend_from_slice(lv);
        })?;
        if oversized {
            return Err(Error::Corrupted("leaf exceeds dump length limits"));
        }
        Ok(out)
    }

    /// Import a dump into an empty tree (Go `ImportDump`), batching adds.
    pub fn import_dump(&mut self, b: &[u8]) -> Result<(), Error> {
        if self.root() != EMPTY_HASH {
            return Err(Error::TreeNotEmpty);
        }
        let mut kvs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut i = 0usize;
        while i < b.len() {
            if b.len() - i < 3 {
                return Err(Error::MalformedDump("truncated record header"));
            }
            let lk = b[i] as usize;
            let lv = u16::from_le_bytes([b[i + 1], b[i + 2]]) as usize;
            i += 3;
            if b.len() - i < lk + lv {
                return Err(Error::MalformedDump("truncated record body"));
            }
            kvs.push((b[i..i + lk].to_vec(), b[i + lk..i + lk + lv].to_vec()));
            i += lk + lv;
        }
        for chunk in kvs.chunks(IMPORT_KEYS_PER_BATCH) {
            // Go ignores per-key invalids here; a dump of a valid tree has none.
            self.add_batch(chunk)?;
        }
        Ok(())
    }
}
