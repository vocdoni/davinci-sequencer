//! HTTP wire types of the sequencer API. Field names are camelCase
//! like davinci-node. Field elements are decimal strings, bytes are `0x` hex,
//! vote ids are `0x` + 16 hex (the BE8 integer), points are TE `{x, y}`.
//! Decoding only accepts canonical values: field elements below p, points on
//! the curve, exact byte lengths.

use std::fmt;
use std::str::FromStr;

use davinci_zkvm_sdk::ballot::{Ballot, BallotMode};
use davinci_zkvm_sdk::census::{CensusProof, CensusWitness, CspProof, EcdsaSignature};
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::limits::{SMT_LEVELS, VOTE_ID_MIN};
use davinci_zkvm_sdk::types::SnarkJsProof;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::Error;
use crate::organizer::OnchainProcess;

pub use davinci_zkvm_sdk::crypto::field::Fr;

/// 31-byte on-chain process id (`bytes31`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ProcessId(pub [u8; 31]);

impl ProcessId {
    /// The pid as a field element: its big-endian integer (248 bits, below p).
    pub fn to_fr(&self) -> Fr {
        use ark_ff::PrimeField;
        Fr::from_be_bytes_mod_order(&self.0)
    }
}

impl fmt::Display for ProcessId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&to_hex(&self.0))
    }
}

impl FromStr for ProcessId {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        parse_hex(s).map(ProcessId).map_err(Error::Invalid)
    }
}

impl Serialize for ProcessId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ProcessId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// A vote id as `0x` + 16 hex digits.
pub fn vote_id_hex(v: u64) -> String {
    format!("0x{v:016x}")
}

/// Inverse of [`vote_id_hex`]; `0x` is optional, the 16 digits are not, and
/// the value must be a vote id (`>= 2^63`).
pub fn parse_vote_id(s: &str) -> Result<u64, Error> {
    let v = parse_hex::<8>(s)
        .map(u64::from_be_bytes)
        .map_err(Error::Invalid)?;
    if v < VOTE_ID_MIN {
        return Err(Error::Invalid("vote id below 2^63".into()));
    }
    Ok(v)
}

fn to_hex(b: &[u8]) -> String {
    format!("0x{}", hex::encode(b))
}

fn parse_hex<const N: usize>(s: &str) -> Result<[u8; N], String> {
    let h = s.strip_prefix("0x").unwrap_or(s);
    let mut out = [0u8; N];
    hex::decode_to_slice(h, &mut out).map_err(|_| format!("want {N} bytes of hex"))?;
    Ok(out)
}

/// serde adapters for the encodings above, usable with `#[serde(with)]`.
pub mod enc {
    use davinci_zkvm_sdk::crypto::field::{fr_from_dec, fr_to_dec};
    use serde::de::Error as _;

    use super::*;

    fn dec_u128(s: &str) -> Result<u128, String> {
        if s.is_empty() || s.len() > 39 || !s.bytes().all(|c| c.is_ascii_digit()) {
            return Err("want a decimal integer string".into());
        }
        s.parse().map_err(|_| "integer out of range".into())
    }

    fn fr_dec_parse(s: &str) -> Result<Fr, String> {
        fr_from_dec(s).map_err(|e| e.to_string())
    }

    /// Fr as a decimal string, below p.
    pub mod fr_dec {
        use super::*;
        pub fn serialize<S: Serializer>(x: &Fr, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_str(&fr_to_dec(x))
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Fr, D::Error> {
            fr_dec_parse(&String::deserialize(d)?).map_err(D::Error::custom)
        }
    }

    /// A list of Fr as decimal strings.
    pub mod fr_dec_vec {
        use super::*;
        pub fn serialize<S: Serializer>(x: &[Fr], s: S) -> Result<S::Ok, S::Error> {
            s.collect_seq(x.iter().map(fr_to_dec))
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Fr>, D::Error> {
            Vec::<String>::deserialize(d)?
                .iter()
                .map(|s| fr_dec_parse(s).map_err(D::Error::custom))
                .collect()
        }
    }

    /// u128 as a decimal string (weights).
    pub mod u128_dec {
        use super::*;
        pub fn serialize<S: Serializer>(x: &u128, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_str(&x.to_string())
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
            dec_u128(&String::deserialize(d)?).map_err(D::Error::custom)
        }
    }

    /// u64 as a decimal string (values beyond 2^53).
    pub mod u64_dec {
        use super::*;
        pub fn serialize<S: Serializer>(x: &u64, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_str(&x.to_string())
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
            let v = dec_u128(&String::deserialize(d)?).map_err(D::Error::custom)?;
            u64::try_from(v).map_err(|_| D::Error::custom("integer out of range"))
        }
    }

    /// Fixed-size bytes as `0x` hex.
    pub mod hex {
        use super::*;
        pub fn serialize<S: Serializer, const N: usize>(
            x: &[u8; N],
            s: S,
        ) -> Result<S::Ok, S::Error> {
            s.serialize_str(&to_hex(x))
        }
        pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(
            d: D,
        ) -> Result<[u8; N], D::Error> {
            parse_hex(&String::deserialize(d)?).map_err(D::Error::custom)
        }
    }

    /// Optional fixed-size bytes as `0x` hex or `null`.
    pub mod hex_opt {
        use super::*;
        pub fn serialize<S: Serializer, const N: usize>(
            x: &Option<[u8; N]>,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            match x {
                Some(b) => s.serialize_some(&to_hex(b)),
                None => s.serialize_none(),
            }
        }
        pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(
            d: D,
        ) -> Result<Option<[u8; N]>, D::Error> {
            Option::<String>::deserialize(d)?
                .map(|s| parse_hex(&s).map_err(D::Error::custom))
                .transpose()
        }
    }

    /// A list of 32-byte values as `0x` hex.
    pub mod hex32_vec {
        use super::*;
        pub fn serialize<S: Serializer>(x: &[[u8; 32]], s: S) -> Result<S::Ok, S::Error> {
            s.collect_seq(x.iter().map(|b| to_hex(b)))
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<[u8; 32]>, D::Error> {
            Vec::<String>::deserialize(d)?
                .iter()
                .map(|s| parse_hex(s).map_err(D::Error::custom))
                .collect()
        }
    }

    /// A list of variable-length byte strings as `0x` hex.
    pub mod hex_bytes_vec {
        use super::*;
        pub fn serialize<S: Serializer>(x: &[Vec<u8>], s: S) -> Result<S::Ok, S::Error> {
            s.collect_seq(x.iter().map(|b| to_hex(b)))
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Vec<u8>>, D::Error> {
            Vec::<String>::deserialize(d)?
                .iter()
                .map(|s| {
                    ::hex::decode(s.strip_prefix("0x").unwrap_or(s))
                        .map_err(|_| D::Error::custom("bad hex"))
                })
                .collect()
        }
    }

    /// Vote id as `0x` + 16 hex.
    pub mod vote_id {
        use super::*;
        pub fn serialize<S: Serializer>(x: &u64, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_str(&vote_id_hex(*x))
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
            parse_vote_id(&String::deserialize(d)?).map_err(D::Error::custom)
        }
    }

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct PointDec {
        x: String,
        y: String,
    }

    impl PointDec {
        fn new(p: &Point) -> Self {
            PointDec {
                x: fr_to_dec(&p.x),
                y: fr_to_dec(&p.y),
            }
        }

        fn point(&self) -> Result<Point, String> {
            let p = Point {
                x: fr_dec_parse(&self.x)?,
                y: fr_dec_parse(&self.y)?,
            };
            if !p.is_on_curve() {
                return Err("point not on the curve".into());
            }
            Ok(p)
        }
    }

    /// TE point as `{x, y}` decimal strings, on the curve.
    pub mod point {
        use super::*;
        pub fn serialize<S: Serializer>(p: &Point, s: S) -> Result<S::Ok, S::Error> {
            PointDec::new(p).serialize(s)
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Point, D::Error> {
            PointDec::deserialize(d)?.point().map_err(D::Error::custom)
        }
    }

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct CiphertextDec {
        c1: PointDec,
        c2: PointDec,
    }

    /// Ballot as exactly 16 `{c1: {x, y}, c2: {x, y}}`.
    pub mod ballot {
        use davinci_zkvm_sdk::crypto::elgamal::Ciphertext;

        use super::*;
        pub fn serialize<S: Serializer>(b: &Ballot, s: S) -> Result<S::Ok, S::Error> {
            s.collect_seq(b.0.iter().map(|c| CiphertextDec {
                c1: PointDec::new(&c.c1),
                c2: PointDec::new(&c.c2),
            }))
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Ballot, D::Error> {
            let cts = <[CiphertextDec; 16]>::deserialize(d)?;
            let mut out = Ballot::identity();
            for (o, c) in out.0.iter_mut().zip(cts.iter()) {
                *o = Ciphertext {
                    c1: c.c1.point().map_err(D::Error::custom)?,
                    c2: c.c2.point().map_err(D::Error::custom)?,
                };
            }
            Ok(out)
        }
    }

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct BallotModeJson {
        num_fields: u8,
        group_size: u8,
        unique_values: bool,
        cost_exponent: u8,
        #[serde(with = "u64_dec")]
        max_value: u64,
        #[serde(with = "u64_dec")]
        min_value: u64,
        #[serde(with = "u64_dec")]
        max_value_sum: u64,
        #[serde(with = "u64_dec")]
        min_value_sum: u64,
    }

    /// Ballot mode in camelCase; it must pack (the circuit's bit widths).
    pub mod ballot_mode {
        use super::*;
        pub fn serialize<S: Serializer>(m: &BallotMode, s: S) -> Result<S::Ok, S::Error> {
            BallotModeJson {
                num_fields: m.num_fields,
                group_size: m.group_size,
                unique_values: m.unique_values,
                cost_exponent: m.cost_exponent,
                max_value: m.max_value,
                min_value: m.min_value,
                max_value_sum: m.max_value_sum,
                min_value_sum: m.min_value_sum,
            }
            .serialize(s)
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BallotMode, D::Error> {
            let j = BallotModeJson::deserialize(d)?;
            let m = BallotMode {
                num_fields: j.num_fields,
                group_size: j.group_size,
                unique_values: j.unique_values,
                cost_exponent: j.cost_exponent,
                max_value: j.max_value,
                min_value: j.min_value,
                max_value_sum: j.max_value_sum,
                min_value_sum: j.min_value_sum,
            };
            m.pack().map_err(D::Error::custom)?;
            Ok(m)
        }
    }
}

/// Merkle (lean-IMT) census proof; `pathBits` are the compact path bits.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MerkleProof {
    #[serde(with = "enc::fr_dec")]
    pub root: Fr,
    #[serde(with = "enc::fr_dec")]
    pub leaf: Fr,
    pub path_bits: u64,
    #[serde(with = "enc::fr_dec_vec")]
    pub siblings: Vec<Fr>,
}

impl From<&CensusProof> for MerkleProof {
    fn from(p: &CensusProof) -> Self {
        MerkleProof {
            root: p.root,
            leaf: p.leaf,
            path_bits: p.path_bits,
            siblings: p.siblings.clone(),
        }
    }
}

impl MerkleProof {
    pub fn to_census_proof(&self) -> CensusProof {
        CensusProof {
            root: self.root,
            leaf: self.leaf,
            path_bits: self.path_bits,
            siblings: self.siblings.clone(),
        }
    }
}

/// CSP attestation over `(pid, address, weight, index)`; the address and
/// weight are the vote's own.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CspWire {
    #[serde(with = "enc::hex")]
    pub r: [u8; 32],
    #[serde(with = "enc::hex")]
    pub s: [u8; 32],
    pub recid: u8,
    pub index: u64,
}

/// Census proof sent with a vote, tagged by `type`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum CensusProofWire {
    Merkle(MerkleProof),
    Csp(CspWire),
}

/// `POST /votes` body: the davinci-node vote package adapted to the zkVM.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VoteRequest {
    pub process_id: ProcessId,
    #[serde(with = "enc::hex")]
    pub address: [u8; 20],
    #[serde(with = "enc::vote_id")]
    pub vote_id: u64,
    #[serde(with = "enc::ballot")]
    pub ballot: Ballot,
    pub ballot_proof: SnarkJsProof,
    #[serde(with = "enc::fr_dec")]
    pub ballot_inputs_hash: Fr,
    /// `r || s || v` over the vote id (personal-sign of `PadToSign(BE8(vid))`).
    #[serde(with = "enc::hex")]
    pub signature: [u8; 65],
    #[serde(with = "enc::u128_dec")]
    pub weight: u128,
    /// Required for CSP. Optional for a Merkle census: the sequencer
    /// re-derives the proof from its own tree and ignores this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub census_proof: Option<CensusProofWire>,
}

impl VoteRequest {
    pub fn signature(&self) -> EcdsaSignature {
        let mut r = [0u8; 32];
        let mut s = [0u8; 32];
        r.copy_from_slice(&self.signature[..32]);
        s.copy_from_slice(&self.signature[32..64]);
        EcdsaSignature {
            r,
            s,
            v: self.signature[64],
        }
    }

    /// The supplied census witness for the SDK checks; a CSP attestation is
    /// read as covering this vote's address and weight.
    pub fn census_witness(&self) -> Option<CensusWitness> {
        Some(match self.census_proof.as_ref()? {
            CensusProofWire::Merkle(m) => CensusWitness::Merkle(m.to_census_proof()),
            CensusProofWire::Csp(c) => CensusWitness::Csp(CspProof {
                r: c.r,
                s: c.s,
                recid: c.recid,
                address: self.address,
                weight: self.weight,
                index: c.index,
            }),
        })
    }
}

/// `POST /processes/keys` body: the (future) process the key is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EncryptionKeyRequest {
    pub process_id: ProcessId,
}

/// `POST /processes/keys` response: the node's election key for that process,
/// TE `{x, y}` decimal strings.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct EncryptionKeyResponse {
    #[serde(with = "enc::fr_dec")]
    pub x: Fr,
    #[serde(with = "enc::fr_dec")]
    pub y: Fr,
}

impl EncryptionKeyResponse {
    pub fn from_point(p: &Point) -> Self {
        EncryptionKeyResponse { x: p.x, y: p.y }
    }

    /// The key, which must be on the curve.
    pub fn point(&self) -> Result<Point, Error> {
        let p = Point {
            x: self.x,
            y: self.y,
        };
        if !p.is_on_curve() {
            return Err(Error::Decode("encryption key not on the curve".into()));
        }
        Ok(p)
    }
}

/// `POST /votes` success.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoteResponse {
    #[serde(with = "enc::vote_id")]
    pub vote_id: u64,
}

/// Body of every error response.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
    pub code: u32,
}

/// Vote lifecycle (davinci-node names): `aggregated` = in a batch,
/// `processed` = proven, `settled` = on-chain.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoteStatus {
    Pending,
    Aggregated,
    Processed,
    Settled,
    Error,
}

impl VoteStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            VoteStatus::Pending => "pending",
            VoteStatus::Aggregated => "aggregated",
            VoteStatus::Processed => "processed",
            VoteStatus::Settled => "settled",
            VoteStatus::Error => "error",
        }
    }
}

impl fmt::Display for VoteStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for VoteStatus {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        Ok(match s {
            "pending" => VoteStatus::Pending,
            "aggregated" => VoteStatus::Aggregated,
            "processed" => VoteStatus::Processed,
            "settled" => VoteStatus::Settled,
            "error" => VoteStatus::Error,
            _ => return Err(Error::Invalid(format!("unknown vote status {s:.20}"))),
        })
    }
}

/// `GET /votes/{pid}/voteId/{vid}`; `error` only with status `error`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct VoteStatusResponse {
    pub status: VoteStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `GET /votes/{pid}/voteId/{vid}/proof`: arbo inclusion proof of the
/// vote-id leaf (value 0) under a settled root; siblings run root to leaf.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackerProof {
    pub process_id: ProcessId,
    #[serde(with = "enc::vote_id")]
    pub vote_id: u64,
    /// Raw arbo root, as stored on-chain in `latestStateRoot`.
    #[serde(with = "enc::hex")]
    pub root: [u8; 32],
    #[serde(with = "enc::hex32_vec")]
    pub siblings: Vec<[u8; 32]>,
}

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// Recorded-as-cast: the vote id is a leaf of the tree whose root is on-chain.
/// Leaf `sha256(vid_le8 || 0^32 || 0x01)`, nodes `sha256(l || r)`, path bits
/// LSB-first from the key.
pub fn verify_tracker(p: &TrackerProof, onchain_root: &[u8; 32]) -> bool {
    if p.root != *onchain_root || p.siblings.len() > SMT_LEVELS || p.vote_id < VOTE_ID_MIN {
        return false;
    }
    let mut node = sha256(&[&p.vote_id.to_le_bytes(), &[0u8; 32], &[1]]);
    for (i, s) in p.siblings.iter().enumerate().rev() {
        node = if (p.vote_id >> i) & 1 == 1 {
            sha256(&[s, &node])
        } else {
            sha256(&[&node, s])
        };
    }
    node == p.root
}

/// `GET /info`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    /// Settling account; `null` in observer mode.
    #[serde(with = "enc::hex_opt")]
    pub sequencer_address: Option<[u8; 20]>,
    pub chain_id: u64,
    #[serde(with = "enc::hex")]
    pub process_registry: [u8; 20],
    #[serde(with = "enc::hex")]
    pub ballot_vk_hash: [u8; 32],
    #[serde(with = "enc::hex")]
    pub batch_program_vk: [u8; 32],
    #[serde(with = "enc::hex")]
    pub results_program_vk: [u8; 32],
    pub observer: bool,
    pub settled_by_self: u64,
    pub synced_from_others: u64,
    /// Batches lost to another sequencer settling first.
    #[serde(default)]
    pub lost_races: u64,
}

/// On-chain process status (the registry enum, lowercase).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcessStatus {
    Ready,
    Ended,
    Canceled,
    Paused,
    Results,
    /// The sequencer never managed to read the on-chain status
    /// (an ignored process bootstrapped from a failing registry read).
    /// Pinned to the sequencer's placeholder ordinal 255 so an `as u8`
    /// cast can never collide with a future registry status 5.
    Unknown = 255,
}

impl ProcessStatus {
    /// From the `DAVINCITypes.ProcessStatus` ordinal.
    pub fn from_onchain(v: u8) -> Option<Self> {
        Some(match v {
            0 => ProcessStatus::Ready,
            1 => ProcessStatus::Ended,
            2 => ProcessStatus::Canceled,
            3 => ProcessStatus::Paused,
            4 => ProcessStatus::Results,
            255 => ProcessStatus::Unknown,
            _ => return None,
        })
    }
}

/// Census of a process: origins 1 to 3 (Merkle, lean-IMT root) or 4 (CSP,
/// root = CSP address as an integer).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CensusView {
    pub census_origin: u8,
    #[serde(with = "enc::fr_dec")]
    pub census_root: Fr,
    #[serde(rename = "censusURI")]
    pub census_uri: String,
}

/// `GET /processes/{pid}`: the on-chain parameters plus the node's view.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessView {
    pub id: ProcessId,
    pub status: ProcessStatus,
    pub is_accepting_votes: bool,
    #[serde(with = "enc::hex")]
    pub organization_id: [u8; 20],
    #[serde(with = "enc::point")]
    pub encryption_key: Point,
    #[serde(with = "enc::ballot_mode")]
    pub ballot_mode: BallotMode,
    pub census: CensusView,
    /// Latest settled root (raw arbo digest).
    #[serde(with = "enc::hex")]
    pub state_root: [u8; 32],
    /// This node's committed local tree root (raw arbo digest); may lead
    /// `state_root` while a transition is in flight. Absent on old nodes.
    #[serde(
        default,
        with = "enc::hex_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub local_state_root: Option<[u8; 32]>,
    /// This node's committed root equals the on-chain root. Absent on old nodes.
    #[serde(default)]
    pub synced: bool,
    /// Votes queued on this node, not yet in a sealed batch. Zero with
    /// `next_seal_not_before` absent means the queue is idle. Absent on
    /// old nodes and where no actor owns the process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_votes: Option<u64>,
    /// Unix seconds: earliest instant the open batch can seal, for the
    /// queue as it stands. New votes only bring it forward, and it is no
    /// deadline — the ±10% jitter is private and gates (an in-flight
    /// batch, the proving budget) can hold the seal past it. Absent when
    /// nothing is pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_seal_not_before: Option<u64>,
    pub voters_count: u64,
    pub overwritten_votes_count: u64,
    pub max_voters: u64,
    /// Unix seconds.
    pub start_time: u64,
    /// Seconds.
    pub duration: u64,
    /// Tally, once the results are on-chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Vec<u64>>,
    /// This node refused to serve the process; the record may be a
    /// placeholder if its on-chain read never succeeded.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ignored: bool,
    /// Why, when `ignored`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ProcessView {
    /// Checks the election parameters a voter relies on (pid, key, ballot mode,
    /// census origin and root) against the registry. Status, counters and the
    /// root may lag the chain and are not compared.
    pub fn check_against(&self, chain: &OnchainProcess) -> Result<(), Error> {
        let differs = if self.id.0 != chain.id {
            "process id"
        } else if self.encryption_key != chain.encryption_key {
            "encryption key"
        } else if self.ballot_mode != chain.ballot_mode {
            "ballot mode"
        } else if self.census.census_origin != chain.census_origin {
            "census origin"
        } else if self.census.census_root != chain.census_root {
            "census root"
        } else {
            return Ok(());
        };
        Err(Error::Decode(format!(
            "process view differs from the registry: {differs}"
        )))
    }
}

/// `GET /processes`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ProcessList {
    pub processes: Vec<ProcessId>,
}

/// `GET /processes/{pid}/participants/{address}`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParticipantResponse {
    #[serde(with = "enc::hex")]
    pub address: [u8; 20],
    #[serde(with = "enc::u128_dec")]
    pub weight: u128,
    pub census_proof: MerkleProof,
}

/// `GET /votes/{pid}/address/{address}`: the stored (re-encrypted) ballot.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BallotResponse {
    #[serde(with = "enc::hex")]
    pub address: [u8; 20],
    #[serde(with = "enc::ballot")]
    pub ballot: Ballot,
}

/// One settled transition of the archive.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransitionView {
    pub index: u64,
    #[serde(with = "enc::hex")]
    pub old_root: [u8; 32],
    #[serde(with = "enc::hex")]
    pub new_root: [u8; 32],
    #[serde(with = "enc::hex")]
    pub tx_hash: [u8; 32],
    pub block_number: u64,
    #[serde(with = "enc::hex")]
    pub sender: [u8; 20],
    pub voters: u64,
    pub overwrites: u64,
    pub n_blobs: u64,
}

/// `GET /processes/{pid}/transitions`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TransitionList {
    pub transitions: Vec<TransitionView>,
}

/// `GET /processes/{pid}/transitions/{i}/blobs`: raw EIP-4844 blobs.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BlobsResponse {
    #[serde(with = "enc::hex_bytes_vec")]
    pub blobs: Vec<Vec<u8>>,
}

/// Census file served at `censusURI` (davinci-node format).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CensusFile {
    pub participants: Vec<CensusParticipant>,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CensusParticipant {
    /// Voter address.
    #[serde(with = "enc::hex")]
    pub key: [u8; 20],
    #[serde(with = "enc::u128_dec")]
    pub weight: u128,
}
