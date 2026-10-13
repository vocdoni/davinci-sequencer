# Deployments

Contract addresses and pinned values of the deployments the `--network` presets point at. A
release that moves a preset to a new registry updates this file and `client/src/networks.rs`
together.

## OP Mainnet

Chain id 10, preset `optimism`, L1-blob mode: transitions settle on OP Mainnet and their blobs
live in Ethereum blob transactions. Deployed on 2026-10-12 from davinci-contracts commit
`e7de916`; davinci-contracts' `deployments/10.json` holds the verified record. Not
source-verified on an explorer yet.

| Contract | Address | Block |
|---|---|---|
| ZiskVerifier | `0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c` (reused; code hash the pinned `0x82385a40…5566`) | |
| L1BlobVerifier | `0x3B95BC14F43ed8Addb83F84D10A949De74eE7529` | 158125136 |
| ProcessRegistry | `0x8b63fAE96948D69CF92Eb40e4EE0fCE40962267d` | 158125136 |

Pins are the v0.1 release values listed under [Retired deployments](#retired-deployments). No
DKG or Council adapter: those key modes are not on OP Stack chains yet. The grace window is the
production default (180 s, floor 150 s, ceiling 600 s, at most 1800 s past the end; 60 s shorten
notice).

`L1BlobVerifier` reads OP's L1 origin from the `L1Block` predeploy
`0x4200000000000000000000000000000000000015`. Consumers of OP transitions (SDKs, explorers) read
`ProcessStateTransitionedL1` for the Ethereum blocks and versioned hashes; there is no `BLOBHASH`
on OP. The preset reads Ethereum from `https://ethereum-rpc.publicnode.com` and
`https://eth.drpc.org`: publicnode refuses request bodies above about 1 MB, dRPC takes the 1.5 MB
six-blob sends, and the node sends a blob transaction to every endpoint. Followers fetch blobs
from `https://ethereum-beacon-api.publicnode.com`, which served full blobs for recent and day-old
slots when the preset was filled in; production nodes should add a beacon node of their own that
keeps full blobs.

## Retired deployments

These registries stay on chain, but no preset follows them. They pin the v0.1 program vks
(davinci-zkvm v0.1.1), so a node of this release, which proves under v0.2.0, refuses them at boot.
Their datadir directories stay untouched next to the current one.

### Gnosis Chain

Chain id 100. The `gnosis` preset followed registry R2 (the production beta) from release v0.5.0
to v0.5.x, and R1 before; release v0.6.0 removed it. All contracts are source-verified on
gnosisscan.io; davinci-contracts' `deployments/100.json` holds R2's verified record.

| Registry | ProcessRegistry | Block | Adapters | DKGManager |
|---|---|---|---|---|
| R2 | `0x20b96e465CA7C3536B9C733571ec1eCf42b2eA21` | 48633301 | DKG `0xB74270Af067Bd75e30cC0cc6D24786EbD6816919`, Council `0x33e91518521Feb5D2A14928563dc6930Bc2F0755` (manager `0x2f5b110864cbad4017fe8ac59111812278f5f71f`) | `0xC6Fb38c42ed3FB35D363a702218746d5C7Da36BF` |
| R1 | `0x6702e0141B6b72bCF8C1bdff20A82A35C5502E7D` | 48504090 | DKG `0xE9559c78E7ff8c19937A0657a092A221E90CCBC3` | `0x9999f38ff8bf959e98ddd5d4551f82775219c01b` |

Both use `ZiskVerifier` `0x150547716bD6f15D872508b66b2ae7ce17677C9C` (block 48504089). A node
for R2 runs release v0.5.0 (its default `gnosis` preset) with a davinci-zkvm v0.1.1 prover; for R1
it also sets `DAVINCI_REGISTRY=0x6702e0141B6b72bCF8C1bdff20A82A35C5502E7D` and
`DAVINCI_START_BLOCK=48504090`.

### v0.1 pins

| Value | Where | Hash |
|---|---|---|
| batch program vk | registry `batchProgramVK` | `0x6cfc89d562d0b22f04478a5c15b390433eb52f1b03147030b183076260da7a10` |
| results program vk | registry `resultsProgramVK` | `0x7bc8c5e9235548386a44b1885732a2a7ffb1badddc8c7fba599d07ece47be794` |
| `rootCVadcopFinal` | registry and verifier | `0x05006517b6ccde5da4d890587ba62845b5af8a307c00e87d4b9d05099b16dc80` |
| ballot VK hash | registry `ballotVKHash`, state leaf `0x07` | `0xbf1e6590bb1ba883d601c4d7d1c6fa2722a78590716874019db6d68fc776bb0e` |
| verifier code keccak256 | `ZiskVerifier` runtime code | `0x82385a405b7301345d7e246017846ca3228aaea349cb68b116d76e0e77056566` |

## Verifying a deployment

Three checks refuse a registry that pins other programs: the node's startup check,
`davinci_client::organizer::verify_registry` for clients, and davinci-contracts'
`script/verify_deployment.py`, which compares the deployed runtime code with a local build (of
the commit in the deployment record) and reads back every pin, the grace settings
(`--grace`) and, in L1-blob mode, the verifier's `L1Block`. Rebuilding a zkVM program changes its
verification key, which needs a new registry.
