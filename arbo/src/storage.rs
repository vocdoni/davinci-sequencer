use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::error::Error;

/// An ordered set of puts applied atomically by `Storage::write`.
/// Nodes are content-addressed and never deleted, so there is no delete op.
#[derive(Default, Debug)]
pub struct WriteBatch {
    puts: Vec<(Vec<u8>, Vec<u8>)>,
}

impl WriteBatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.puts.push((key, value));
    }

    pub fn len(&self) -> usize {
        self.puts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.puts.is_empty()
    }

    pub fn into_puts(self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.puts
    }
}

/// Key-value storage for tree nodes and metadata.
pub trait Storage: Send + Sync {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error>;
    /// Apply the batch atomically.
    fn write(&self, batch: WriteBatch) -> Result<(), Error>;
}

/// In-memory storage backed by a shared hash map.
#[derive(Clone, Default)]
pub struct MemoryStorage {
    map: Arc<RwLock<HashMap<Vec<u8>, Vec<u8>>>>,
}

impl MemoryStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Storage for MemoryStorage {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        let map = self.map.read().map_err(|e| Error::Storage(e.to_string()))?;
        Ok(map.get(key).cloned())
    }

    fn write(&self, batch: WriteBatch) -> Result<(), Error> {
        let mut map = self
            .map
            .write()
            .map_err(|e| Error::Storage(e.to_string()))?;
        for (k, v) in batch.into_puts() {
            map.insert(k, v);
        }
        Ok(())
    }
}

#[cfg(feature = "redb")]
pub use redb_storage::RedbStorage;

#[cfg(feature = "redb")]
mod redb_storage {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use redb::{Database, Durability, ReadTransaction, ReadableDatabase, TableDefinition};

    use super::{Storage, WriteBatch};
    use crate::error::Error;

    /// Tree storage in a redb table. The database handle is shared, so one
    /// redb file can host several trees under different table names.
    ///
    /// Reads of content-addressed node keys reuse one cached snapshot,
    /// refreshed on every write through this handle (clones share the
    /// cache) and on any miss. Mutable metadata (root/nleafs/params) is
    /// always read from a fresh transaction, so a separately constructed
    /// handle on the same table never observes a stale root.
    #[derive(Clone)]
    pub struct RedbStorage {
        db: Arc<Database>,
        table: String,
        durability: Durability,
        snapshot: Arc<Mutex<Option<ReadTransaction>>>,
    }

    fn map_err<E: std::fmt::Display>(e: E) -> Error {
        Error::Storage(e.to_string())
    }

    impl RedbStorage {
        /// Open a tree table on a shared redb database, creating it if missing.
        pub fn new(db: Arc<Database>, table: &str) -> Result<Self, Error> {
            let s = Self {
                db,
                table: table.to_string(),
                durability: Durability::Immediate,
                snapshot: Arc::new(Mutex::new(None)),
            };
            // Create the table so reads before the first write don't fail.
            let tx = s.db.begin_write().map_err(map_err)?;
            {
                let def: TableDefinition<&[u8], &[u8]> = TableDefinition::new(&s.table);
                tx.open_table(def).map_err(map_err)?;
            }
            tx.commit().map_err(map_err)?;
            Ok(s)
        }

        /// Create or open a redb file at `path` and use `table` in it.
        pub fn open(path: impl AsRef<Path>, table: &str) -> Result<Self, Error> {
            let db = Database::create(path).map_err(map_err)?;
            Self::new(Arc::new(db), table)
        }

        /// Trade commit durability for speed (`Durability::None` keeps the
        /// file consistent but may lose the latest commits on crash).
        pub fn set_durability(&mut self, durability: Durability) {
            self.durability = durability;
        }
    }

    impl Storage for RedbStorage {
        fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
            // Only 32-byte content-addressed node keys are immutable and
            // safe to serve from the cached snapshot. Anything else
            // (root/nleafs/params metadata) is mutable and must be read
            // fresh so a second handle never sees a stale value.
            if key.len() != 32 {
                let tx = self.db.begin_read().map_err(map_err)?;
                let def: TableDefinition<&[u8], &[u8]> = TableDefinition::new(&self.table);
                let table = tx.open_table(def).map_err(map_err)?;
                return Ok(table.get(key).map_err(map_err)?.map(|v| v.value().to_vec()));
            }
            let mut snap = self
                .snapshot
                .lock()
                .map_err(|_| Error::Storage("poisoned snapshot lock".into()))?;
            let mut fresh = snap.is_none();
            loop {
                if snap.is_none() {
                    *snap = Some(self.db.begin_read().map_err(map_err)?);
                }
                let tx = snap.as_ref().expect("snapshot just set");
                let def: TableDefinition<&[u8], &[u8]> = TableDefinition::new(&self.table);
                let table = tx.open_table(def).map_err(map_err)?;
                let out = table.get(key).map_err(map_err)?;
                if let Some(v) = out {
                    return Ok(Some(v.value().to_vec()));
                }
                // Content-addressed nodes are never deleted, so a miss on a
                // reused snapshot may just mean another handle wrote since;
                // refresh once and retry.
                if fresh {
                    return Ok(None);
                }
                *snap = None;
                fresh = true;
            }
        }

        fn write(&self, batch: WriteBatch) -> Result<(), Error> {
            // Drop the read snapshot first: an open snapshot pins the old
            // tree version and makes the commit CoW more pages.
            *self
                .snapshot
                .lock()
                .map_err(|_| Error::Storage("poisoned snapshot lock".into()))? = None;
            // Sorted insertion is cheaper for the b-tree than random order.
            let mut puts = batch.into_puts();
            if puts.len() >= 4096 {
                use rayon::slice::ParallelSliceMut;
                puts.par_sort_unstable_by(|a, b| a.0.cmp(&b.0));
            } else {
                puts.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            }
            let mut tx = self.db.begin_write().map_err(map_err)?;
            tx.set_durability(self.durability).map_err(map_err)?;
            {
                let def: TableDefinition<&[u8], &[u8]> = TableDefinition::new(&self.table);
                let mut table = tx.open_table(def).map_err(map_err)?;
                for (k, v) in puts {
                    table.insert(k.as_slice(), v.as_slice()).map_err(map_err)?;
                }
            }
            tx.commit().map_err(map_err)?;
            // Clear again: a get racing the commit could have re-opened a
            // pre-commit snapshot.
            *self
                .snapshot
                .lock()
                .map_err(|_| Error::Storage("poisoned snapshot lock".into()))? = None;
            Ok(())
        }
    }
}
