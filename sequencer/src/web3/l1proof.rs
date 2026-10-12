//! Proof that blob transactions are in Ethereum's canonical chain, for the L1-blob DA mode: RLP
//! headers newest first (from about the L1 head down to each blob block) and a Merkle-Patricia
//! proof of each transaction in its block's transactions trie. `L1BlobVerifier` checks the same
//! thing on OP against `L1Block.hash()` or a recorded origin; `verify_local` mirrors it before a send.
//!
//! `verify_local` is a pre-flight, not the oracle. On non-honest inputs it differs from the
//! contract: a block number longer than 8 bytes is rejected (the contract truncates it), the last
//! proof node must be a leaf, there is no 32-item RLP list cap, error kinds and their order
//! differ, and there is no `MAX_AGE` check.

use alloy::consensus::Header;
use alloy::eips::eip2718::Encodable2718;
use alloy::primitives::{B256, Bytes, keccak256};
use alloy::providers::Provider;
use alloy::rlp::{Decodable, Encodable, Header as RlpHeader};
use alloy::trie::proof::{ProofRetainer, verify_proof};
use alloy::trie::{HashBuilder, Nibbles};

/// Merkle-Patricia proof of one transaction in the block of `headers[header_index]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct L1TxProof {
    pub header_index: usize,
    /// `rlp(txIndex)`.
    pub key: Bytes,
    /// Trie nodes from the root down to the leaf.
    pub proof: Vec<Bytes>,
}

/// Everything `L1BlobVerifier.verify` takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct L1BlobProof {
    /// RLP headers, newest first, each the parent of the one before.
    pub headers: Vec<Bytes>,
    pub txs: Vec<L1TxProof>,
}

#[derive(Debug, thiserror::Error)]
pub enum L1ProofError {
    #[error("header of block {0} does not hash to its block hash")]
    HeaderHash(u64),
    #[error("transactions root of block {0} does not match its header")]
    TxRoot(u64),
    #[error("header chain broken at position {0}")]
    BrokenChain(usize),
    #[error("no header matches the L1 origin {0}")]
    NoAnchor(B256),
    #[error("transaction {0} references a header above the origin")]
    AboveOrigin(usize),
    #[error("transaction {0} is not a blob transaction")]
    NotBlobTx(usize),
    #[error("bad header: {0}")]
    BadHeader(String),
    #[error("bad transaction proof: {0}")]
    BadProof(String),
    #[error("rpc: {0}")]
    Rpc(String),
}

/// Transactions-trie key of `index`: `rlp(index)` (`0x80` for 0).
pub fn tx_key(index: usize) -> Bytes {
    alloy::rlp::encode(index as u64).into()
}

/// RLP of `h`, checked against `hash`.
pub fn header_rlp(h: &Header, hash: B256) -> Result<Bytes, L1ProofError> {
    let mut out = Vec::new();
    h.encode(&mut out);
    if keccak256(&out) != hash {
        return Err(L1ProofError::HeaderHash(h.number));
    }
    Ok(out.into())
}

/// Root of the transactions trie over `envelopes` (EIP-2718, block order) and the proof of `index`.
pub fn tx_trie_proof(envelopes: &[Bytes], index: usize) -> (B256, Vec<Bytes>) {
    let mut items: Vec<(Nibbles, &Bytes)> = envelopes
        .iter()
        .enumerate()
        .map(|(i, e)| (Nibbles::unpack(tx_key(i)), e))
        .collect();
    items.sort_by_key(|a| a.0);
    let target = Nibbles::unpack(tx_key(index));
    let mut hb = HashBuilder::default().with_proof_retainer(ProofRetainer::new(vec![target]));
    for (k, v) in &items {
        hb.add_leaf(*k, v);
    }
    let root = hb.root();
    let proof = hb
        .take_proof_nodes()
        .matching_nodes_sorted(&target)
        .into_iter()
        .map(|(_, n)| n)
        .collect();
    (root, proof)
}

/// Checks that `value` sits at `key` under `root`.
pub fn verify_tx(
    root: B256,
    key: &Bytes,
    proof: &[Bytes],
    value: &Bytes,
) -> Result<(), L1ProofError> {
    verify_proof(
        root,
        Nibbles::unpack(key),
        Some(value.to_vec()),
        proof.iter(),
    )
    .map_err(|e| L1ProofError::BadProof(e.to_string()))
}

/// The whole encoded items of an RLP list that spans all of `buf`.
fn rlp_items(buf: &[u8]) -> Option<Vec<&[u8]>> {
    let mut b = buf;
    let h = RlpHeader::decode(&mut b).ok()?;
    if !h.list || h.payload_length != b.len() {
        return None;
    }
    let mut items = vec![];
    while !b.is_empty() {
        let start = b;
        let h = RlpHeader::decode(&mut b).ok()?;
        b = b.get(h.payload_length..)?;
        items.push(&start[..start.len() - b.len()]);
    }
    Some(items)
}

/// The payload of an encoded RLP string item.
fn rlp_str(item: &[u8]) -> Option<&[u8]> {
    let mut b = item;
    let h = RlpHeader::decode(&mut b).ok()?;
    (!h.list).then_some(b)
}

/// The fields of a header RLP the verifier reads by index: (parentHash, transactionsRoot, number).
fn header_fields(rlp: &[u8]) -> Result<(B256, B256, u64), L1ProofError> {
    let bad = |m: &str| L1ProofError::BadHeader(m.to_string());
    let items = rlp_items(rlp).ok_or_else(|| bad("not an rlp list"))?;
    if items.len() < 9 {
        return Err(bad("fewer than 9 items"));
    }
    let hash = |i: usize| {
        rlp_str(items[i])
            .filter(|b| b.len() == 32)
            .map(B256::from_slice)
            .ok_or_else(|| bad("hash is not 32 bytes"))
    };
    let number = rlp_str(items[8])
        .filter(|b| b.len() <= 8)
        .ok_or_else(|| bad("bad block number"))?;
    let number = number.iter().fold(0u64, |n, b| n << 8 | *b as u64);
    Ok((hash(0)?, hash(4)?, number))
}

/// Versioned hashes of a type-3 envelope (`0x03 ++ rlp([.., blobVersionedHashes, ..])`), or
/// `None` for anything else.
fn blob_hashes(envelope: &[u8]) -> Option<Vec<B256>> {
    let (&ty, body) = envelope.split_first()?;
    let items = rlp_items(body).filter(|i| ty == 0x03 && i.len() >= 11)?;
    let hashes = rlp_items(items[10])?;
    if hashes.is_empty() {
        return None;
    }
    hashes
        .into_iter()
        .map(|h| {
            rlp_str(h)
                .filter(|b| b.len() == 32 && b[0] == 0x01)
                .map(B256::from_slice)
        })
        .collect()
}

/// Mirrors `L1BlobVerifier.verify`: the versioned hashes in proof order and, parallel to them,
/// the L1 block of the transaction holding each one. `anchor` is the hash the chain must reach:
/// OP's current `L1Block.hash()` or a recorded origin hash.
pub fn verify_local(p: &L1BlobProof, anchor: B256) -> Result<(Vec<B256>, Vec<u64>), L1ProofError> {
    let mut fields: Vec<(B256, B256, u64)> = Vec::with_capacity(p.headers.len());
    let mut anchor_at = None;
    for (i, h) in p.headers.iter().enumerate() {
        let f = header_fields(h)?;
        let hash = keccak256(h);
        if i > 0 && fields.last().map(|(parent, _, _)| *parent) != Some(hash) {
            return Err(L1ProofError::BrokenChain(i));
        }
        if anchor_at.is_none() && hash == anchor {
            anchor_at = Some(i);
        }
        fields.push(f);
    }
    let anchor_at = anchor_at.ok_or(L1ProofError::NoAnchor(anchor))?;
    let (mut vhs, mut blocks) = (vec![], vec![]);
    for (t, tx) in p.txs.iter().enumerate() {
        if tx.header_index < anchor_at || tx.header_index >= p.headers.len() {
            return Err(L1ProofError::AboveOrigin(t));
        }
        let (_, root, number) = fields[tx.header_index];
        let value = proven_value(root, &tx.key, &tx.proof)?;
        let hashes = blob_hashes(&value).ok_or(L1ProofError::NotBlobTx(t))?;
        blocks.extend(std::iter::repeat_n(number, hashes.len()));
        vhs.extend(hashes);
    }
    Ok((vhs, blocks))
}

/// The leaf value of a proof: the last node is the leaf; its value is the envelope.
fn proven_value(root: B256, key: &Bytes, proof: &[Bytes]) -> Result<Bytes, L1ProofError> {
    let leaf = proof
        .last()
        .ok_or_else(|| L1ProofError::BadProof("empty proof".into()))?;
    let node = alloy::trie::nodes::TrieNode::decode(&mut &leaf[..])
        .map_err(|e| L1ProofError::BadProof(e.to_string()))?;
    let value = match node {
        alloy::trie::nodes::TrieNode::Leaf(l) => Bytes::from(l.value),
        _ => return Err(L1ProofError::BadProof("last node is not a leaf".into())),
    };
    verify_tx(root, key, proof, &value)?;
    Ok(value)
}

/// Builds the proof for the blob transactions `tx_hashes` (in that order) with headers from block
/// `top` (inclusive, at or above OP's L1 origin; the L1 head is a good choice) down to the oldest
/// blob block.
pub async fn build<P: Provider>(
    l1: &P,
    tx_hashes: &[B256],
    top: u64,
) -> Result<L1BlobProof, L1ProofError> {
    let rpc = |e: alloy::transports::TransportError| L1ProofError::Rpc(e.to_string());
    let mut located = vec![];
    for h in tx_hashes {
        let r = l1
            .get_transaction_receipt(*h)
            .await
            .map_err(rpc)?
            .ok_or_else(|| L1ProofError::Rpc(format!("no receipt for {h}")))?;
        let n = r
            .block_number
            .ok_or_else(|| L1ProofError::Rpc(format!("{h} not mined")))?;
        let i = r
            .transaction_index
            .ok_or_else(|| L1ProofError::Rpc(format!("{h} has no index")))?
            as usize;
        located.push((n, i));
    }
    let oldest = located
        .iter()
        .map(|(n, _)| *n)
        .min()
        .ok_or_else(|| L1ProofError::Rpc("no transactions".into()))?;
    if top < oldest {
        return Err(L1ProofError::Rpc(format!(
            "top {top} is below block {oldest}"
        )));
    }
    let mut headers = vec![];
    for b in (oldest..=top).rev() {
        let blk = l1
            .get_block_by_number(b.into())
            .await
            .map_err(rpc)?
            .ok_or_else(|| L1ProofError::Rpc(format!("no block {b}")))?;
        headers.push(header_rlp(&blk.header.inner, blk.header.hash)?);
    }
    let mut txs = vec![];
    for (n, i) in located {
        let blk = l1
            .get_block_by_number(n.into())
            .full()
            .await
            .map_err(rpc)?
            .ok_or_else(|| L1ProofError::Rpc(format!("no block {n}")))?;
        let envs: Vec<Bytes> = blk
            .transactions
            .txns()
            .map(|t| t.inner.encoded_2718().into())
            .collect();
        let (root, proof) = tx_trie_proof(&envs, i);
        if root != blk.header.transactions_root {
            return Err(L1ProofError::TxRoot(n));
        }
        txs.push(L1TxProof {
            header_index: (top - n) as usize,
            key: tx_key(i),
            proof,
        });
    }
    Ok(L1BlobProof { headers, txs })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RLP list of already-encoded items.
    fn list(items: &[Vec<u8>]) -> Vec<u8> {
        let payload: Vec<u8> = items.concat();
        let mut out = vec![];
        RlpHeader {
            list: true,
            payload_length: payload.len(),
        }
        .encode(&mut out);
        out.extend(payload);
        out
    }

    fn string(b: &[u8]) -> Vec<u8> {
        alloy::rlp::encode(b)
    }

    fn envelope(ty: u8, first_byte: u8) -> Vec<u8> {
        let mut vh = [7u8; 32];
        vh[0] = first_byte;
        let mut items: Vec<Vec<u8>> = (0..10).map(|i| string(&[i])).collect();
        items.push(list(&[string(&vh)]));
        items.push(string(b"sig"));
        [vec![ty], list(&items)].concat()
    }

    #[test]
    fn header_fields_by_index() {
        let h = Header {
            number: 123,
            parent_hash: B256::repeat_byte(1),
            transactions_root: B256::repeat_byte(2),
            ..Default::default()
        };
        let mut rlp = vec![];
        h.encode(&mut rlp);
        assert_eq!(
            header_fields(&rlp).unwrap(),
            (B256::repeat_byte(1), B256::repeat_byte(2), 123)
        );
        let short = list(&(0..8).map(|i| string(&[i])).collect::<Vec<_>>());
        assert!(matches!(
            header_fields(&short),
            Err(L1ProofError::BadHeader(_))
        ));
        assert!(matches!(
            header_fields(&rlp[..rlp.len() - 1]),
            Err(L1ProofError::BadHeader(_))
        ));
    }

    #[test]
    fn blob_hashes_by_index() {
        let got = blob_hashes(&envelope(3, 1)).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0][0], 1);
        assert_eq!(blob_hashes(&envelope(3, 2)), None);
        assert_eq!(blob_hashes(&envelope(2, 1)), None);
        assert_eq!(blob_hashes(&[]), None);
    }

    #[test]
    fn tx_key_encodings() {
        assert_eq!(tx_key(0).as_ref(), &[0x80]);
        assert_eq!(tx_key(1).as_ref(), &[0x01]);
        assert_eq!(tx_key(127).as_ref(), &[0x7f]);
        assert_eq!(tx_key(128).as_ref(), &[0x81, 0x80]);
        assert_eq!(tx_key(255).as_ref(), &[0x81, 0xff]);
        assert_eq!(tx_key(256).as_ref(), &[0x82, 0x01, 0x00]);
    }

    #[test]
    fn trie_proofs_verify_for_every_index() {
        // 300 envelopes, so keys of one, two and three bytes all occur.
        let envs: Vec<Bytes> = (0..300u32)
            .map(|i| {
                Bytes::from(
                    [0x02u8]
                        .into_iter()
                        .chain(i.to_be_bytes())
                        .chain([7u8; 100])
                        .collect::<Vec<u8>>(),
                )
            })
            .collect();
        let want = alloy::trie::root::ordered_trie_root_with_encoder(&envs, |e, buf| {
            buf.extend_from_slice(e)
        });
        for i in [0usize, 1, 127, 128, 255, 256, 299] {
            let (root, proof) = tx_trie_proof(&envs, i);
            assert_eq!(root, want, "root for index {i}");
            assert!(
                verify_tx(root, &tx_key(i), &proof, &envs[i]).is_ok(),
                "proof for index {i}"
            );
        }
    }
}
