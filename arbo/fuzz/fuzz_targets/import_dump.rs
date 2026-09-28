//! import_dump must never panic on arbitrary dump bytes.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut t = arbo::Tree::new(arbo::MemoryStorage::new(), 64, arbo::Sha256).unwrap();
    let _ = t.import_dump(data);
});
