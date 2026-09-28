//! Corrupted storage bytes must surface as errors, never panics: overwrite
//! the root node with fuzz bytes, fabricate intermediate chains, and
//! exercise every read/write path.
#![no_main]
use arbo::{MemoryStorage, Sha256, Storage, Tree, WriteBatch};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let storage = MemoryStorage::new();
    let mut t = Tree::new(storage.clone(), 64, Sha256).unwrap();
    t.add(&1u64.to_le_bytes(), &[7u8; 32]).unwrap();
    t.add(&2u64.to_le_bytes(), &[8u8; 32]).unwrap();
    let root = t.root();

    // Replace the root node's stored bytes with fuzz data.
    let mut wb = WriteBatch::new();
    wb.put(root.to_vec(), data.to_vec());
    storage.write(wb).unwrap();

    exercise(&mut t, &root);

    // Fabricate an intermediate chain: node i's left child is node i+1,
    // right child and depth taken from the fuzz input.
    let depth = (data.first().copied().unwrap_or(0) as usize % 80) + 1;
    let mut right = [0u8; 32];
    for (i, b) in data.iter().skip(1).take(32).enumerate() {
        right[i] = *b;
    }
    let mut wb = WriteBatch::new();
    for i in 0..depth {
        let mut key = [0xC5u8; 32];
        key[0] = i as u8;
        let mut next = [0xC5u8; 32];
        next[0] = i as u8 + 1;
        let mut v = vec![2u8, 32];
        v.extend_from_slice(&next);
        v.extend_from_slice(&right);
        wb.put(key.to_vec(), v);
    }
    let mut head = [0xC5u8; 32];
    head[0] = 0;
    wb.put(b"root".to_vec(), head.to_vec());
    storage.write(wb).unwrap();

    let mut t = Tree::new(storage, 64, Sha256).unwrap();
    exercise(&mut t, &head);
});

fn exercise(t: &mut Tree<MemoryStorage, Sha256>, root: &[u8; 32]) {
    let _ = t.get(&1u64.to_le_bytes());
    let _ = t.gen_proof(&1u64.to_le_bytes());
    let _ = t.add(&3u64.to_le_bytes(), &[9u8; 32]);
    let _ = t.update(&1u64.to_le_bytes(), &[6u8; 32]);
    let _ = t.dump(root);
    let _ = t.add_batch(&[(4u64.to_le_bytes().to_vec(), vec![5u8; 32])]);
    let _ = t.iterate(root, |_k, _v| {});
}
