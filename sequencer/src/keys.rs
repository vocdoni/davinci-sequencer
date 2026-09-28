//! Election keys (`POST /processes/keys`). A key is a function of the node
//! master secret and the process id, so handing one out stores nothing. The
//! master secret sits in the `enc_keys` table of the 0600 redb file and is
//! never logged.

use std::sync::Arc;

use davinci_zkvm_sdk::crypto::babyjubjub::{Point, SUBGROUP_ORDER};
use davinci_zkvm_sdk::crypto::field::{U256, u256_from_le, u256_to_le};
use hmac::{Hmac, Mac};
use num_bigint::BigUint;
use rand::RngCore;
use rand::rngs::OsRng;
use sha2::Sha256;
use tracing::info;
use zeroize::Zeroizing;

use crate::storage::{Db, Result};

/// Domain tag of the key derivation.
const TAG: &[u8] = b"davinci-election-key-v1";

/// Derives the election keys of one chain and registry.
#[derive(Clone)]
pub struct KeyStore {
    master: Arc<Zeroizing<[u8; 32]>>,
    chain_id: u64,
    registry: [u8; 20],
}

// Never prints the master secret.
impl std::fmt::Debug for KeyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyStore")
            .field("chain_id", &self.chain_id)
            .field("registry", &hex::encode(self.registry))
            .finish_non_exhaustive()
    }
}

impl KeyStore {
    /// Loads the node master secret, drawing it from the OS RNG on first boot.
    pub fn open(db: &Db, chain_id: u64, registry: [u8; 20]) -> Result<Self> {
        let master = db.master_secret(|| {
            // A wrong datadir shows up here: its keys are not the old ones.
            info!("drew a new election-key master secret");
            let mut b = [0u8; 32];
            OsRng.fill_bytes(&mut b);
            b
        })?;
        Ok(Self::from_master(master, chain_id, registry))
    }

    fn from_master(master: [u8; 32], chain_id: u64, registry: [u8; 20]) -> Self {
        KeyStore {
            master: Arc::new(Zeroizing::new(master)),
            chain_id,
            registry,
        }
    }

    /// `sk = HMAC-SHA256(master, TAG ‖ chain_id_be8 ‖ registry20 ‖ pid31 ‖ i)`
    /// for `i = 0, 1`, read as one 64-byte BE integer, mod `l - 1`, plus 1.
    /// The wide reduction keeps the bias below 2^-260 and `sk` in `[1, l)`.
    pub fn secret(&self, pid: &[u8; 31]) -> U256 {
        let mut wide = Zeroizing::new([0u8; 64]);
        for (i, half) in wide.chunks_exact_mut(32).enumerate() {
            // HMAC takes keys of any length, so this cannot fail.
            let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.master[..])
                .expect("HMAC accepts any key length");
            mac.update(TAG);
            mac.update(&self.chain_id.to_be_bytes());
            mac.update(&self.registry);
            mac.update(pid);
            mac.update(&[i as u8]);
            half.copy_from_slice(&mac.finalize().into_bytes());
        }
        // The BigUint and the returned U256 are not zeroized; accepted because
        // the master itself stays in memory for the node's lifetime.
        let l_minus_1 = BigUint::from_bytes_le(&u256_to_le(&SUBGROUP_ORDER)) - 1u32;
        let sk = BigUint::from_bytes_be(&wide[..]) % l_minus_1 + 1u32;
        // sk < l < 2^251 fits in 32 bytes.
        let mut le = Zeroizing::new([0u8; 32]);
        let bytes = Zeroizing::new(sk.to_bytes_le());
        le[..bytes.len()].copy_from_slice(&bytes);
        u256_from_le(&le)
    }

    /// The election public key for `pid`, `sk * B8` as `keygen` computes it.
    pub fn public_key(&self, pid: &[u8; 31]) -> Point {
        Point::generator().mul(&self.secret(pid))
    }

    /// The secret for `pid` if `enc_key` is this node's key for it; `None`
    /// means another node (or nobody) holds it.
    pub fn secret_for(&self, pid: &[u8; 31], enc_key: &Point) -> Option<U256> {
        let sk = self.secret(pid);
        (Point::generator().mul(&sk) == *enc_key).then_some(sk)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    const REG: [u8; 20] = [0x11; 20];

    fn pid(b: u8) -> [u8; 31] {
        [b; 31]
    }

    fn store(dir: &tempfile::TempDir) -> KeyStore {
        KeyStore::open(&Db::open(&dir.path().join("s.redb")).unwrap(), 1, REG).unwrap()
    }

    #[test]
    fn same_inputs_same_key_across_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sequencer.redb");
        let (sk, pk) = {
            let ks = KeyStore::open(&Db::open(&path).unwrap(), 1, REG).unwrap();
            assert_eq!(ks.secret(&pid(1)), ks.secret(&pid(1)));
            (ks.secret(&pid(1)), ks.public_key(&pid(1)))
        };
        // The master secret survives a reopen, so the key does too.
        let db = Db::open(&path).unwrap();
        let ks = KeyStore::open(&db, 1, REG).unwrap();
        assert_eq!(ks.secret(&pid(1)), sk);
        assert_eq!(ks.public_key(&pid(1)), pk);
        assert_eq!(db.enc_key_count().unwrap(), 1);
        // The file holding the secret is private.
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn every_input_changes_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.redb")).unwrap();
        let base = KeyStore::open(&db, 1, REG).unwrap().secret(&pid(1));
        assert_ne!(KeyStore::open(&db, 2, REG).unwrap().secret(&pid(1)), base);
        assert_ne!(
            KeyStore::open(&db, 1, [0x12; 20]).unwrap().secret(&pid(1)),
            base
        );
        let mut p = pid(1);
        p[30] ^= 1;
        assert_ne!(KeyStore::open(&db, 1, REG).unwrap().secret(&p), base);
        // Another node's master secret.
        let other = tempfile::tempdir().unwrap();
        assert_ne!(store(&other).secret(&pid(1)), base);
    }

    #[test]
    fn key_is_in_range_and_matches_the_scalar_mul() {
        let dir = tempfile::tempdir().unwrap();
        let ks = store(&dir);
        let zero = U256::from(0u64);
        for b in 0..64u8 {
            let sk = ks.secret(&pid(b));
            assert_ne!(sk, zero);
            assert!(sk < SUBGROUP_ORDER);
            let pk = ks.public_key(&pid(b));
            assert_eq!(pk, Point::generator().mul(&sk));
            assert!(pk != Point::IDENTITY && pk.in_subgroup());
        }
    }

    #[test]
    fn secret_for_only_matches_own_key() {
        let dir = tempfile::tempdir().unwrap();
        let ks = store(&dir);
        let pk = ks.public_key(&pid(1));
        assert_eq!(ks.secret_for(&pid(1), &pk), Some(ks.secret(&pid(1))));
        // Our key for another process, and another node's key for this one.
        assert_eq!(ks.secret_for(&pid(2), &pk), None);
        let other = tempfile::tempdir().unwrap();
        assert_eq!(
            ks.secret_for(&pid(1), &store(&other).public_key(&pid(1))),
            None
        );
    }

    #[test]
    fn existing_file_is_tightened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sequencer.redb");
        drop(Db::open(&path).unwrap());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        drop(Db::open(&path).unwrap());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// Pins the exact derivation (vector recomputed independently in Python).
    #[test]
    fn known_answer_vector() {
        use davinci_zkvm_sdk::crypto::field::fr_to_be;
        let ks = KeyStore::from_master([0x42; 32], 31337, [0x11; 20]);
        let sk = ks.secret(&[0x5a; 31]);
        assert_eq!(
            hex::encode(u256_to_le(&sk)),
            "9b00a84253de0a8ae8175262a514b3e8f53f17c29b2f8788eb957d46a387ab04"
        );
        let pk = ks.public_key(&[0x5a; 31]);
        assert_eq!(
            hex::encode(fr_to_be(&pk.x)),
            "0c57b42057a9f12445d954244e5345b9fbe056cb589adf056d385f87f8159159"
        );
        assert_eq!(
            hex::encode(fr_to_be(&pk.y)),
            "2bfb2e815a46616846579bb6fcc4611c94b6790ea988c9e92cf27354c2b18274"
        );
    }

    #[test]
    fn debug_has_no_secret() {
        let dir = tempfile::tempdir().unwrap();
        let ks = store(&dir);
        let dbg = format!("{ks:?}");
        assert_eq!(
            dbg,
            format!(
                "KeyStore {{ chain_id: 1, registry: \"{}\", .. }}",
                hex::encode(REG)
            )
        );
        assert!(!dbg.contains(&hex::encode(&ks.master[..])));
    }
}
