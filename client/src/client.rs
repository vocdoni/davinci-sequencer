//! HTTP client for every sequencer route.

use std::time::Duration;

use davinci_zkvm_sdk::census::{CensusProof, census_leaf, verify_census_proof};
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::api::{
    self, BallotResponse, BlobsResponse, EncryptionKeyRequest, EncryptionKeyResponse,
    ErrorResponse, Info, ParticipantResponse, ProcessId, ProcessList, ProcessView, TrackerProof,
    TransitionList, TransitionView, VoteRequest, VoteResponse, VoteStatus, VoteStatusResponse,
};
use crate::{Error, Result};

/// Largest response body read (six blobs as hex are ~1.6 MB).
const MAX_BODY: usize = 16 << 20;
/// Longest error text kept from a response.
const MAX_ERR: usize = 4096;

#[derive(Clone, Debug)]
pub struct SequencerClient {
    base: String,
    http: reqwest::Client,
}

fn api_error(status: u16, body: &[u8]) -> Error {
    let (message, code) = match serde_json::from_slice::<ErrorResponse>(body) {
        Ok(e) => (e.error, Some(e.code)),
        Err(_) => (String::from_utf8_lossy(body).into_owned(), None),
    };
    Error::Api {
        status,
        code,
        message: message.chars().take(MAX_ERR).collect(),
    }
}

async fn read_body(mut resp: reqwest::Response) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| Error::Http(e.to_string()))? {
        if out.len() + chunk.len() > MAX_BODY {
            return Err(Error::Decode("response body too large".into()));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn hex_addr(a: &[u8; 20]) -> String {
    format!("0x{}", hex::encode(a))
}

impl SequencerClient {
    /// Client with a 10 s connect and 60 s request timeout.
    pub fn new(url: &str) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self::with_http(url, http)
    }

    pub fn with_http(url: &str, http: reqwest::Client) -> Self {
        SequencerClient {
            base: url.trim_end_matches('/').to_string(),
            http,
        }
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Vec<u8>> {
        let resp = req.send().await.map_err(|e| Error::Http(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = read_body(resp).await?;
        if !(200..300).contains(&status) {
            return Err(api_error(status, &body));
        }
        Ok(body)
    }

    fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
        serde_json::from_slice(body).map_err(|e| Error::Decode(e.to_string()))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let body = self
            .send(self.http.get(format!("{}{path}", self.base)))
            .await?;
        Self::decode(&body)
    }

    async fn post<B: Serialize, T: DeserializeOwned>(&self, path: &str, b: &B) -> Result<T> {
        let body = self
            .send(self.http.post(format!("{}{path}", self.base)).json(b))
            .await?;
        Self::decode(&body)
    }

    pub async fn ping(&self) -> Result<()> {
        self.send(self.http.get(format!("{}/ping", self.base)))
            .await
            .map(|_| ())
    }

    pub async fn info(&self) -> Result<Info> {
        self.get("/info").await
    }

    /// Asks the sequencer for its election key for process `pid` (usually the
    /// registry's `getNextProcessId`). It must be a prime-order, non-identity
    /// point, as the ballot circuit requires.
    pub async fn new_key(&self, pid: &[u8; 31]) -> Result<Point> {
        let req = EncryptionKeyRequest {
            process_id: ProcessId(*pid),
        };
        let pk = self
            .post::<_, EncryptionKeyResponse>("/processes/keys", &req)
            .await?
            .point()?;
        if pk == Point::IDENTITY || !pk.in_subgroup() {
            return Err(Error::Decode(
                "encryption key is not a prime-order point".into(),
            ));
        }
        Ok(pk)
    }

    pub async fn processes(&self) -> Result<Vec<ProcessId>> {
        Ok(self.get::<ProcessList>("/processes").await?.processes)
    }

    pub async fn process(&self, pid: &[u8; 31]) -> Result<ProcessView> {
        self.get(&format!("/processes/{}", ProcessId(*pid))).await
    }

    /// Census proof and weight of `addr`. The proof must be for `addr`'s own
    /// leaf and verify; the root is left to the caller (see `Voter::build_vote`).
    pub async fn participant(
        &self,
        pid: &[u8; 31],
        addr: &[u8; 20],
    ) -> Result<(CensusProof, u128)> {
        let r: ParticipantResponse = self
            .get(&format!(
                "/processes/{}/participants/{}",
                ProcessId(*pid),
                hex_addr(addr)
            ))
            .await?;
        let proof = r.census_proof.to_census_proof();
        let leaf = census_leaf(addr, r.weight).map_err(|e| Error::Decode(e.to_string()))?;
        if r.address != *addr || proof.leaf != leaf || !verify_census_proof(&proof) {
            return Err(Error::Decode(
                "participant proof is not for this address".into(),
            ));
        }
        Ok((proof, r.weight))
    }

    pub async fn submit_vote(&self, v: &VoteRequest) -> Result<()> {
        let r: VoteResponse = self.post("/votes", v).await?;
        if r.vote_id != v.vote_id {
            return Err(Error::Decode(
                "sequencer acknowledged another vote id".into(),
            ));
        }
        Ok(())
    }

    pub async fn vote_status(&self, pid: &[u8; 31], vid: u64) -> Result<VoteStatus> {
        Ok(self.vote_status_full(pid, vid).await?.status)
    }

    /// Status with the error text of an `error` vote.
    pub async fn vote_status_full(&self, pid: &[u8; 31], vid: u64) -> Result<VoteStatusResponse> {
        self.get(&format!(
            "/votes/{}/voteId/{}",
            ProcessId(*pid),
            api::vote_id_hex(vid)
        ))
        .await
    }

    /// Tracker proof; check it with [`api::verify_tracker`] against the root
    /// read from the chain, not the one in the proof.
    pub async fn vote_id_proof(&self, pid: &[u8; 31], vid: u64) -> Result<TrackerProof> {
        let p: TrackerProof = self
            .get(&format!(
                "/votes/{}/voteId/{}/proof",
                ProcessId(*pid),
                api::vote_id_hex(vid)
            ))
            .await?;
        if p.vote_id != vid || p.process_id.0 != *pid {
            return Err(Error::Decode("tracker proof for another vote".into()));
        }
        Ok(p)
    }

    pub async fn ballot(&self, pid: &[u8; 31], addr: &[u8; 20]) -> Result<BallotResponse> {
        self.get(&format!(
            "/votes/{}/address/{}",
            ProcessId(*pid),
            hex_addr(addr)
        ))
        .await
    }

    pub async fn transitions(&self, pid: &[u8; 31]) -> Result<Vec<TransitionView>> {
        let r: TransitionList = self
            .get(&format!("/processes/{}/transitions", ProcessId(*pid)))
            .await?;
        Ok(r.transitions)
    }

    pub async fn transition_blobs(&self, pid: &[u8; 31], index: u64) -> Result<Vec<Vec<u8>>> {
        let r: BlobsResponse = self
            .get(&format!(
                "/processes/{}/transitions/{index}/blobs",
                ProcessId(*pid)
            ))
            .await?;
        Ok(r.blobs)
    }
}
