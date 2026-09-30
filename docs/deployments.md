# Deployments

Contract addresses and pinned values of the deployments the `--network` presets point at. A
release that moves a preset to a new registry updates this file and `client/src/networks.rs`
together.

## Gnosis Chain

Chain id 100, preset `gnosis`. All contracts are source-verified on gnosisscan.io; earlier
registries on Gnosis are retired.

DAVINCI contracts:

| Contract | Address | Block |
|---|---|---|
| ZiskVerifier | `0x150547716bD6f15D872508b66b2ae7ce17677C9C` | 48504089 |
| ProcessRegistry | `0x6702e0141B6b72bCF8C1bdff20A82A35C5502E7D` | 48504090 |
| DavinciDKGAdapter | `0xE9559c78E7ff8c19937A0657a092A221E90CCBC3` | 48504090 |

DKG contracts:

| Contract | Address | Block |
|---|---|---|
| DKGManager | `0x9999f38ff8bf959e98ddd5d4551f82775219c01b` | 48483860 |
| DKGAppManager | `0xd4d8f9708c380d81aec294b199081c5d2c782087` | 48483862 |
| DKGRegistry | `0x45ab8b64633076ddc020b12d1f1325fa55f629c5` | 48483859 |
| ContributionVerifier | `0x6d198bc613205957444b53a09bb22ed7bc650912` | 48483855 |
| FinalizeVerifier | `0xc354ea7f3ef6db4ca0b89a1a5a6395c2d6126b38` | 48483856 |
| PartialDecryptVerifier | `0x0f19886ee73fd74e3f88ce3a061490facd7561db` | 48483857 |
| DecryptCombineVerifier | `0x2980e664edef91f554cc75b15cb8eeea61586644` | 48483858 |

Pinned values, equal to the SDK release pins:

| Value | Where | Hash |
|---|---|---|
| batch program vk | registry `batchProgramVK` | `0x6cfc89d562d0b22f04478a5c15b390433eb52f1b03147030b183076260da7a10` |
| results program vk | registry `resultsProgramVK` | `0x7bc8c5e9235548386a44b1885732a2a7ffb1badddc8c7fba599d07ece47be794` |
| `rootCVadcopFinal` | registry and verifier | `0x05006517b6ccde5da4d890587ba62845b5af8a307c00e87d4b9d05099b16dc80` |
| ballot VK hash | registry `ballotVKHash`, state leaf `0x07` | `0xbf1e6590bb1ba883d601c4d7d1c6fa2722a78590716874019db6d68fc776bb0e` |
| verifier code keccak256 | `ZiskVerifier` runtime code | `0x82385a405b7301345d7e246017846ca3228aaea349cb68b116d76e0e77056566` |

The DKG committee runs threshold 2 of 3, with epochs of 17280 blocks (a day at 5 s) and automatic
epoch creation when a key pool is spent. Application registration on the DKGAppManager is open, so
other applications can share the committee.

### Verifying the deployment

Three checks refuse a registry that pins other programs: the node's startup check,
`davinci_client::organizer::verify_registry` for clients, and davinci-contracts'
`script/verify_deployment.py`, which compares the deployed runtime code with a local build and
reads back every pin. From a davinci-contracts checkout:

```bash
python3 script/verify_deployment.py --rpc https://rpc.gnosischain.com \
  --registry 0x6702e0141B6b72bCF8C1bdff20A82A35C5502E7D --chain-id 100 \
  --batch-vk  0x6cfc89d562d0b22f04478a5c15b390433eb52f1b03147030b183076260da7a10 \
  --results-vk 0x7bc8c5e9235548386a44b1885732a2a7ffb1badddc8c7fba599d07ece47be794 \
  --root-c 0x05006517b6ccde5da4d890587ba62845b5af8a307c00e87d4b9d05099b16dc80 \
  --ballot-vk-hash 0xbf1e6590bb1ba883d601c4d7d1c6fa2722a78590716874019db6d68fc776bb0e
```

Rebuilding a zkVM program changes its verification key, which needs a new registry. Deploy order:
the DKG contracts (`DeployAll.s.sol` in davinci-dkg), then the `ProcessRegistry` (constructor:
verifier, verification keys, DKG manager), which creates the adapter.

### Chain notes

- Gnosis runs Fusaka: blob transactions carry EIP-7594 cell-proof sidecars. The node detects this
  from `eth_config`, or with the P256VERIFY probe on RPCs that lack it.
- A block takes at most 2 blobs. A batch that needs more is split into several transactions.
- A settlement uses about 400–460k gas and costs almost only its blob fee.
- Some public RPCs serve neither `eth_config` nor `eth_blobBaseFee`; the node falls back to the
  probe, its chain table and `eth_feeHistory`. Public RPCs also rate-limit bursts, so production
  nodes should list their own endpoints first.
