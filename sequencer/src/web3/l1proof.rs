//! Proof that blob transactions are in Ethereum's canonical chain, for the L1-blob DA mode: RLP
//! headers newest first (from about the L1 head down to each blob block) and a Merkle-Patricia
//! proof of each transaction in its block's transactions trie. `L1BlobVerifier` checks the same
//! thing on OP against `L1Block.hash()`; `verify_local` mirrors it before a send.

use alloy::consensus::{Header, Transaction};
use alloy::eips::eip2718::{Decodable2718, Encodable2718};
use alloy::primitives::{B256, Bytes, keccak256};
use alloy::providers::Provider;
use alloy::rlp::{Decodable, Encodable};
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

/// The fields of a header RLP the verifier reads: (parentHash, transactionsRoot, number).
fn header_fields(rlp: &[u8]) -> Result<(B256, B256, u64), L1ProofError> {
    let h = Header::decode(&mut &rlp[..]).map_err(|e| L1ProofError::BadHeader(e.to_string()))?;
    Ok((h.parent_hash, h.transactions_root, h.number))
}

/// Versioned hashes of a type-3 envelope, or `None` for any other type.
fn blob_hashes(envelope: &[u8]) -> Option<Vec<B256>> {
    let tx = alloy::consensus::TxEnvelope::decode_2718(&mut &envelope[..]).ok()?;
    tx.blob_versioned_hashes()
        .map(|v| v.to_vec())
        .filter(|v| !v.is_empty())
}

/// Mirrors `L1BlobVerifier.verify`: the versioned hashes in proof order and the block number of
/// each transaction. `origin` is OP's `L1Block.hash()`.
pub fn verify_local(p: &L1BlobProof, origin: B256) -> Result<(Vec<B256>, Vec<u64>), L1ProofError> {
    let mut fields: Vec<(B256, B256, u64)> = Vec::with_capacity(p.headers.len());
    let mut anchor = None;
    for (i, h) in p.headers.iter().enumerate() {
        let f = header_fields(h)?;
        let hash = keccak256(h);
        if i > 0 && fields.last().map(|(parent, _, _)| *parent) != Some(hash) {
            return Err(L1ProofError::BrokenChain(i));
        }
        if anchor.is_none() && hash == origin {
            anchor = Some(i);
        }
        fields.push(f);
    }
    let anchor = anchor.ok_or(L1ProofError::NoAnchor(origin))?;
    let (mut vhs, mut blocks) = (vec![], vec![]);
    for (t, tx) in p.txs.iter().enumerate() {
        if tx.header_index < anchor || tx.header_index >= p.headers.len() {
            return Err(L1ProofError::AboveOrigin(t));
        }
        let (_, root, number) = fields[tx.header_index];
        let value = proven_value(root, &tx.key, &tx.proof)?;
        vhs.extend(blob_hashes(&value).ok_or(L1ProofError::NotBlobTx(t))?);
        blocks.push(number);
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
