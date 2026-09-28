//! unpack_siblings must never panic on arbitrary bytes.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = arbo::unpack_siblings(&arbo::Sha256, data);
});
