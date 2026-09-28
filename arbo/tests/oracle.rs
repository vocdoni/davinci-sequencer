//! Test oracle: verbatim port of the zkVM guest's SMT verifier
//! (`davinci-zkvm/circuit-primitives/src/smt.rs`, `verify_transition` /
//! `verify_inclusion`), with the ZisK sha256 syscall replaced by the `sha2`
//! crate. Used by `processor.rs` to check that the processor proofs the arbo
//! crate produces are exactly what the guest accepts.
//!
//! This file carries no tests of its own; cargo builds it as an empty test
//! crate and `processor.rs` includes it via `#[path]`.
#![allow(dead_code)]

use sha2::{Digest, Sha256};

pub type FrRaw = [u64; 4];

pub struct SmtTransition {
    pub old_root: FrRaw,
    pub new_root: FrRaw,
    pub old_key: FrRaw,
    pub old_value: FrRaw,
    pub is_old0: bool,
    pub new_key: FrRaw,
    pub new_value: FrRaw,
    pub fnc0: bool,
    pub fnc1: bool,
    /// Merkle siblings, root→leaf order, padded to `n_levels` with zeros.
    pub siblings: Vec<FrRaw>,
}

fn sha256_once(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

// Byte-order helpers

/// FrRaw (LE word order) → little-endian 32 bytes (Arbo's byte format).
pub fn fr_to_le(v: &FrRaw) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[0..8].copy_from_slice(&v[0].to_le_bytes());
    out[8..16].copy_from_slice(&v[1].to_le_bytes());
    out[16..24].copy_from_slice(&v[2].to_le_bytes());
    out[24..32].copy_from_slice(&v[3].to_le_bytes());
    out
}

/// Little-endian 32 bytes → FrRaw (LE word order).
pub fn le_to_fr(b: &[u8; 32]) -> FrRaw {
    [
        u64::from_le_bytes(b[0..8].try_into().unwrap()),
        u64::from_le_bytes(b[8..16].try_into().unwrap()),
        u64::from_le_bytes(b[16..24].try_into().unwrap()),
        u64::from_le_bytes(b[24..32].try_into().unwrap()),
    ]
}

// Arbo-compatible hash functions

/// Arbo leaf hash: `SHA256(key_le8 || value_le32 || 0x01)` => 41 bytes.
/// The tree has `SMT_LEVELS = 64` levels, so arbo keys are 8 bytes; only
/// limb 0 of the key is ever hashed or walked.
pub fn leaf_hash(key: &FrRaw, value: &FrRaw) -> FrRaw {
    let mut input = [0u8; 41];
    input[0..8].copy_from_slice(&key[0].to_le_bytes());
    input[8..40].copy_from_slice(&fr_to_le(value));
    input[40] = 0x01;
    le_to_fr(&sha256_once(&input))
}

/// Arbo internal node hash: `SHA256(left_le32 || right_le32)` => 64 bytes.
pub fn node_hash(left: &FrRaw, right: &FrRaw) -> FrRaw {
    let mut input = [0u8; 64];
    input[0..32].copy_from_slice(&fr_to_le(left));
    input[32..64].copy_from_slice(&fr_to_le(right));
    le_to_fr(&sha256_once(&input))
}

// Path helpers

/// Switcher: `sel=0 → (l, r)`, `sel=1 → (r, l)`.
fn switcher(sel: bool, l: FrRaw, r: FrRaw) -> (FrRaw, FrRaw) {
    if sel { (r, l) } else { (l, r) }
}

/// Get path bit `level` from key (LSB-first, LE word order).
pub fn get_bit(key: &FrRaw, level: usize) -> bool {
    let word_idx = level / 64;
    let bit_idx = level % 64;
    if word_idx >= 4 {
        return false;
    }
    (key[word_idx] >> bit_idx) & 1 == 1
}

// LevIns

/// Detect the insertion level in an SMT Merkle proof.
fn lev_ins_flag(siblings: &[FrRaw], enabled: bool) -> (bool, Vec<bool>) {
    let n = siblings.len();
    if n == 0 {
        return (!enabled, vec![]);
    }
    if n == 1 {
        let valid = if enabled {
            siblings[0] == [0u64; 4]
        } else {
            true
        };
        return (valid, vec![true]);
    }

    let is_zero: Vec<bool> = siblings.iter().map(|s| *s == [0u64; 4]).collect();

    let mut lev_ins = vec![false; n];
    let mut done = vec![false; n - 1];

    lev_ins[n - 1] = !is_zero[n - 2];
    done[n - 2] = lev_ins[n - 1];

    for i in (1..n - 1).rev() {
        lev_ins[i] = !done[i] && !is_zero[i - 1];
        done[i - 1] = lev_ins[i] || done[i];
    }

    lev_ins[0] = !done[0];

    let leaf_zero_ok = is_zero[n - 1];
    let one_hot = lev_ins.iter().filter(|&&x| x).count() == 1;
    let valid = if enabled {
        leaf_zero_ok && one_hot
    } else {
        true
    };

    (valid, lev_ins)
}

// Processor state machine

#[allow(clippy::too_many_arguments)]
fn processor_sm(
    xor: u8,
    is0: u8,
    lev_ins: u8,
    fnc0: u8,
    prev_top: u8,
    prev_old0: u8,
    prev_bot: u8,
    prev_new1: u8,
    prev_na: u8,
    prev_upd: u8,
) -> (u8, u8, u8, u8, u8, u8) {
    let aux1 = prev_top * lev_ins;
    let aux2 = aux1 * fnc0;
    let st_top = prev_top - aux1;
    let st_old0 = aux2 * is0;
    let inner = (aux2 - st_old0) + prev_bot;
    let st_new1 = inner * xor;
    let st_bot = inner * (1 - xor);
    let st_upd = aux1 - aux2;
    let st_na = prev_new1 + prev_old0 + prev_na + prev_upd;
    (st_top, st_old0, st_bot, st_new1, st_na, st_upd)
}

// Processor level

#[allow(clippy::too_many_arguments)]
fn processor_level(
    st_top: u8,
    st_old0: u8,
    st_bot: u8,
    st_new1: u8,
    st_upd: u8,
    sibling: &FrRaw,
    old1leaf: &FrRaw,
    new1leaf: &FrRaw,
    new_lr_bit: bool,
    old_child: &FrRaw,
    new_child: &FrRaw,
) -> (FrRaw, FrRaw) {
    let old_root = if st_top == 1 {
        let (l, r) = switcher(new_lr_bit, *old_child, *sibling);
        node_hash(&l, &r)
    } else if st_bot == 1 || st_new1 == 1 || st_upd == 1 {
        *old1leaf
    } else {
        [0u64; 4]
    };

    let new_root = if st_top == 1 || st_bot == 1 || st_new1 == 1 {
        let left_val = if st_top == 1 || st_bot == 1 {
            *new_child
        } else if st_new1 == 1 {
            *new1leaf
        } else {
            [0u64; 4]
        };
        let right_val = if st_top == 1 {
            *sibling
        } else if st_new1 == 1 {
            *old1leaf
        } else {
            [0u64; 4]
        };
        let (nl, nr) = switcher(new_lr_bit, left_val, right_val);
        node_hash(&nl, &nr)
    } else if st_old0 == 1 || st_upd == 1 {
        *new1leaf
    } else {
        [0u64; 4]
    };

    (old_root, new_root)
}

// Top-level verifier

/// Verify a single SMT state-transition (circomlib `SMTProcessor`).
pub fn verify_transition(t: &SmtTransition) -> bool {
    let levels = t.siblings.len();
    if levels == 0 {
        return false;
    }

    let enabled = t.fnc0 || t.fnc1;

    let (lev_valid, lev_ins) = lev_ins_flag(&t.siblings, enabled);
    if !lev_valid {
        return false;
    }

    let xors: Vec<u8> = (0..levels)
        .map(|i| (get_bit(&t.old_key, i) ^ get_bit(&t.new_key, i)) as u8)
        .collect();

    let is0 = t.is_old0 as u8;
    let fnc0 = t.fnc0 as u8;
    let enabled_u = enabled as u8;

    let mut st_top_v = vec![0u8; levels];
    let mut st_old0_v = vec![0u8; levels];
    let mut st_bot_v = vec![0u8; levels];
    let mut st_new1_v = vec![0u8; levels];
    let mut st_na_v = vec![0u8; levels];
    let mut st_upd_v = vec![0u8; levels];

    for i in 0..levels {
        let (top, old0, bot, new1, na, upd) = if i == 0 {
            processor_sm(
                xors[i],
                is0,
                lev_ins[i] as u8,
                fnc0,
                enabled_u,
                0,
                0,
                0,
                1 - enabled_u,
                0,
            )
        } else {
            processor_sm(
                xors[i],
                is0,
                lev_ins[i] as u8,
                fnc0,
                st_top_v[i - 1],
                st_old0_v[i - 1],
                st_bot_v[i - 1],
                st_new1_v[i - 1],
                st_na_v[i - 1],
                st_upd_v[i - 1],
            )
        };
        st_top_v[i] = top;
        st_old0_v[i] = old0;
        st_bot_v[i] = bot;
        st_new1_v[i] = new1;
        st_na_v[i] = na;
        st_upd_v[i] = upd;
    }

    let last = levels - 1;
    let terminal = st_na_v[last] + st_new1_v[last] + st_old0_v[last] + st_upd_v[last];
    if terminal != 1 {
        return false;
    }

    let need_old = (0..levels).any(|i| (st_bot_v[i] | st_new1_v[i] | st_upd_v[i]) == 1);
    let need_new = (0..levels).any(|i| (st_new1_v[i] | st_old0_v[i] | st_upd_v[i]) == 1);
    let zero_leaf = [0u64; 4];
    let hash1_old = if need_old {
        leaf_hash(&t.old_key, &t.old_value)
    } else {
        zero_leaf
    };
    let hash1_new = if need_new {
        leaf_hash(&t.new_key, &t.new_value)
    } else {
        zero_leaf
    };

    let zero = [0u64; 4];
    let mut levels_old_root = vec![zero; levels];
    let mut levels_new_root = vec![zero; levels];

    for i in (0..levels).rev() {
        let (old_child, new_child) = if i == levels - 1 {
            (zero, zero)
        } else {
            (levels_old_root[i + 1], levels_new_root[i + 1])
        };
        let new_lr_bit = get_bit(&t.new_key, i);
        let (or, nr) = processor_level(
            st_top_v[i],
            st_old0_v[i],
            st_bot_v[i],
            st_new1_v[i],
            st_upd_v[i],
            &t.siblings[i],
            &hash1_old,
            &hash1_new,
            new_lr_bit,
            &old_child,
            &new_child,
        );
        levels_old_root[i] = or;
        levels_new_root[i] = nr;
    }

    let del = t.fnc0 && t.fnc1;
    let (top_l, top_r) = switcher(del, levels_old_root[0], levels_new_root[0]);

    if enabled && top_l != t.old_root {
        return false;
    }

    if !t.fnc0 && t.fnc1 && t.old_key != t.new_key {
        return false;
    }

    let computed_new_root = if enabled { top_r } else { t.old_root };
    computed_new_root == t.new_root
}

// Inclusion verifier (SMTVerifier)

/// Verify SMT **inclusion** of `(key, value)` under `root`.
pub fn verify_inclusion(root: &FrRaw, key: &FrRaw, value: &FrRaw, siblings: &[FrRaw]) -> bool {
    let n = siblings.len();
    if n == 0 {
        return false;
    }

    let (lev_valid, lev_ins) = lev_ins_flag(siblings, true);
    if !lev_valid {
        return false;
    }

    let leaf = leaf_hash(key, value);

    let mut st_top = vec![0u8; n];
    let mut st_new = vec![0u8; n];
    let mut st_na = vec![0u8; n];
    for i in 0..n {
        let (prev_top, prev_new, prev_na) = if i == 0 {
            (1u8, 0u8, 0u8)
        } else {
            (st_top[i - 1], st_new[i - 1], st_na[i - 1])
        };
        let aux1 = prev_top * (lev_ins[i] as u8);
        st_top[i] = prev_top - aux1;
        st_new[i] = aux1;
        st_na[i] = prev_na + prev_new;
    }

    let last = n - 1;
    if st_na[last] + st_new[last] != 1 {
        return false;
    }

    let zero = [0u64; 4];
    let mut levels = vec![zero; n];
    for i in (0..n).rev() {
        let child = if i < n - 1 { levels[i + 1] } else { zero };
        levels[i] = if st_top[i] == 1 {
            let (l, r) = switcher(get_bit(key, i), child, siblings[i]);
            node_hash(&l, &r)
        } else if st_new[i] == 1 {
            leaf
        } else {
            zero
        };
    }

    &levels[0] == root
}
