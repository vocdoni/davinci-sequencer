//! `OwnedCensus` (origin 3) from the davinci-onchain-census-contract forge
//! project: deploy (KeccakLeanIMT, no library to link), add members, read
//! root and size.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use alloy::primitives::{Address, U256, aliases::U88};
use alloy::providers::DynProvider;
use alloy::rpc::types::TransactionReceipt;
use alloy::sol;
use anyhow::{Context, Result, bail, ensure};

use crate::chain::deploy_receipt;

sol!(
    #[sol(rpc, all_derives)]
    #[allow(missing_docs)]
    OwnedCensus,
    "../sequencer/abi/OwnedCensus.json"
);

/// The forge project: `DAVINCI_CENSUS_CONTRACT_DIR`, default
/// `../davinci-onchain-census-contract` next to the workspace (branch
/// `davinci-zkvm`).
pub fn census_dir() -> PathBuf {
    std::env::var_os("DAVINCI_CENSUS_CONTRACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../davinci-onchain-census-contract")
        })
}

fn artifact(dir: &Path, name: &str) -> Result<serde_json::Value> {
    let p = dir.join("out").join(name);
    Ok(serde_json::from_slice(
        &std::fs::read(&p).with_context(|| p.display().to_string())?,
    )?)
}

fn code_of(j: &serde_json::Value) -> Result<String> {
    Ok(j["bytecode"]["object"]
        .as_str()
        .context("artifact without bytecode")?
        .trim_start_matches("0x")
        .to_string())
}

/// An `OwnedCensus` owned by the deploying account.
pub struct Census {
    pub address: Address,
    c: OwnedCensus::OwnedCensusInstance<DynProvider>,
    /// Every mined receipt, reverted ones included.
    receipts: Mutex<Vec<(String, TransactionReceipt)>>,
}

impl Census {
    /// Deploys an `OwnedCensus` (`dir` built) from `provider`'s account; its
    /// KeccakLeanIMT is internal, so there is no library to deploy or link.
    /// Share the provider with any other sender on that account: it caches
    /// the nonce.
    pub async fn deploy(provider: DynProvider, dir: &Path) -> Result<Census> {
        let owned = artifact(dir, "OwnedCensus.sol/OwnedCensus.json")?;
        ensure!(
            owned["bytecode"]["linkReferences"] == serde_json::json!({}),
            "OwnedCensus links a library: rebuild davinci-onchain-census-contract"
        );
        let r = deploy_receipt(&provider, hex::decode(code_of(&owned)?)?)
            .await
            .context("deploy OwnedCensus")?;
        let address = r.contract_address.context("no OwnedCensus address")?;
        Ok(Census {
            address,
            c: OwnedCensus::new(address, provider),
            receipts: Mutex::new(vec![("deploy OwnedCensus".into(), r)]),
        })
    }

    /// An `OwnedCensus` deployed earlier, driven from `provider`'s account.
    pub fn at(address: Address, provider: DynProvider) -> Census {
        Census {
            address,
            c: OwnedCensus::new(address, provider),
            receipts: Mutex::new(Vec::new()),
        }
    }

    /// `(label, receipt)` of every transaction mined so far.
    pub fn receipts(&self) -> Vec<(String, TransactionReceipt)> {
        self.receipts.lock().map(|r| r.clone()).unwrap_or_default()
    }

    fn record(&self, label: String, r: &TransactionReceipt) {
        if let Ok(mut v) = self.receipts.lock() {
            v.push((label, r.clone()));
        }
    }

    /// Adds `members` in one transaction.
    pub async fn add_members(&self, members: &[([u8; 20], u128)]) -> Result<()> {
        let users = members.iter().map(|(a, _)| Address::from(*a)).collect();
        let weights = members.iter().map(|(_, w)| U88::from(*w)).collect();
        let r = self
            .c
            .addMembers(users, weights)
            .send()
            .await?
            .get_receipt()
            .await?;
        self.record(format!("addMembers({})", members.len()), &r);
        ensure!(r.status(), "addMembers reverted");
        Ok(())
    }

    /// `addMember`. A revert is simulated for its ABI error name, then sent
    /// anyway (fixed gas) and must be mined as reverted.
    pub async fn add_member(&self, addr: [u8; 20], weight: u128) -> Result<()> {
        let call = self.c.addMember(Address::from(addr), U88::from(weight));
        let reason = match call.call().await {
            Ok(_) => None,
            Err(e) => match e.as_decoded_interface_error::<OwnedCensus::OwnedCensusErrors>() {
                Some(d) => Some(format!("{d:?}")),
                None => return Err(e.into()),
            },
        };
        let r = call.gas(1_000_000).send().await?.get_receipt().await?;
        self.record("addMember".into(), &r);
        match reason {
            None => ensure!(r.status(), "addMember reverted"),
            Some(d) => {
                ensure!(!r.status(), "addMember simulated {d} but was mined");
                bail!("reverted: {d}")
            }
        }
        Ok(())
    }

    /// The contract's root, big-endian (a 256-bit Keccak value).
    pub async fn root(&self) -> Result<[u8; 32]> {
        let r: U256 = self.c.getCensusRoot().call().await?;
        Ok(r.to_be_bytes::<32>())
    }

    pub async fn size(&self) -> Result<u64> {
        let s: U256 = self.c.treeSize().call().await?;
        Ok(u64::try_from(s)?)
    }
}
