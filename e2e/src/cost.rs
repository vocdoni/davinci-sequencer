//! What a run spent: gas, blob gas and the native token per transaction.

use alloy::primitives::{Address, B256, U256, utils::format_ether};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionReceipt;
use anyhow::{Context, Result};

/// Blob gas of one blob (EIP-4844).
pub const GAS_PER_BLOB: u64 = 131_072;

#[derive(Clone, Debug)]
pub struct TxCost {
    pub label: String,
    pub tx: B256,
    pub from: Address,
    pub block: u64,
    pub gas: u64,
    pub gas_price: u128,
    pub blob_gas: u64,
    pub blob_gas_price: u128,
    pub ok: bool,
}

impl TxCost {
    pub fn of(label: impl Into<String>, r: &TransactionReceipt) -> TxCost {
        TxCost {
            label: label.into(),
            tx: r.transaction_hash,
            from: r.from,
            block: r.block_number.unwrap_or_default(),
            gas: r.gas_used,
            gas_price: r.effective_gas_price,
            blob_gas: r.blob_gas_used.unwrap_or_default(),
            blob_gas_price: r.blob_gas_price.unwrap_or_default(),
            ok: r.status(),
        }
    }

    pub fn blobs(&self) -> u64 {
        self.blob_gas / GAS_PER_BLOB
    }

    /// Execution plus blob fee, in wei.
    pub fn wei(&self) -> U256 {
        U256::from(self.gas) * U256::from(self.gas_price)
            + U256::from(self.blob_gas) * U256::from(self.blob_gas_price)
    }

    pub fn line(&self) -> String {
        format!(
            "{:<34} block {:>9} gas {:>9} @ {} wei, blobs {} @ {} wei, {} {}{}",
            self.label,
            self.block,
            self.gas,
            self.gas_price,
            self.blobs(),
            self.blob_gas_price,
            format_ether(self.wei()),
            self.tx,
            if self.ok { "" } else { " REVERTED" }
        )
    }
}

/// The receipt of `tx`, as a cost line labelled `label`.
pub async fn fetch(url: &str, label: impl Into<String>, tx: B256) -> Result<TxCost> {
    let p = ProviderBuilder::new().connect_client(crate::chain::rpc(url)?);
    let r = p
        .get_transaction_receipt(tx)
        .await?
        .with_context(|| format!("no receipt for {tx}"))?;
    Ok(TxCost::of(label, &r))
}

/// One line per transaction and the total in the native token.
pub fn report(title: &str, costs: &[TxCost]) -> String {
    let total: U256 = costs.iter().map(TxCost::wei).sum();
    let mut out = format!(
        "{title}: {} txs, {} total\n",
        costs.len(),
        format_ether(total)
    );
    for c in costs {
        out.push_str("  ");
        out.push_str(&c.line());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wei_counts_blob_gas() {
        let c = TxCost {
            label: "t".into(),
            tx: B256::ZERO,
            from: Address::ZERO,
            block: 1,
            gas: 500_000,
            gas_price: 9,
            blob_gas: 2 * GAS_PER_BLOB,
            blob_gas_price: 1_000_000_000,
            ok: true,
        };
        assert_eq!(c.blobs(), 2);
        assert_eq!(c.wei(), U256::from(4_500_000u64 + 262_144_000_000_000u64));
        assert!(report("x", &[c]).contains("0.000262144004500000 total"));
    }
}
