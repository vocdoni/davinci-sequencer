//! check_proof must never panic on arbitrary key/value/root/packed bytes.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let kl = data[0] as usize % 40;
    let vl = data[1] as usize % 64;
    let rest = &data[2..];
    if rest.len() < kl + vl + 32 {
        return;
    }
    let k = &rest[..kl];
    let v = &rest[kl..kl + vl];
    let mut root = [0u8; 32];
    root.copy_from_slice(&rest[kl + vl..kl + vl + 32]);
    let packed = &rest[kl + vl + 32..];
    let _ = arbo::check_proof(&arbo::Sha256, k, v, &root, packed);
});
