# Deployments

Contract addresses and pinned values of the deployments the `--network` presets point at. A
release that moves a preset to a new registry updates this file and `client/src/networks.rs`
together.

## Gnosis Chain

Chain id 100, preset `gnosis`: registry R2, the production beta, followed since release v0.5.0.
All contracts are source-verified on gnosisscan.io.

DAVINCI contracts:

| Contract | Address | Block |
|---|---|---|
| ZiskVerifier | `0x150547716bD6f15D872508b66b2ae7ce17677C9C` | 48504089 |
| ProcessRegistry | `0x20b96e465CA7C3536B9C733571ec1eCf42b2eA21` | 48633301 |
| DavinciDKGAdapter | `0xB74270Af067Bd75e30cC0cc6D24786EbD6816919` | 48633301 |
| CouncilAdapter | `0x33e91518521Feb5D2A14928563dc6930Bc2F0755` | 48633301 |

The registry creates both adapters in its constructor and reuses the verifier of R1.

DKG contracts ([davinci-dkg](https://github.com/vocdoni/davinci-dkg)):

| Contract | Address | Block |
|---|---|---|
| DKGManager | `0xC6Fb38c42ed3FB35D363a702218746d5C7Da36BF` | 48632905 |
| DKGAppManager | `0x81bac6b9aae85311741204c22cbaab96f03b567a` | 48632906 |
| DKGRegistry | `0x393049828bc565152c223730ce67574f15a7a16a` | 48632904 |
| ContributionVerifier | `0x67b7c4daa3db8b84e817d1a3d57cb8c40f59935f` | 48632904 |
| FinalizeVerifier | `0x682ffa2a6e049e0dc889ceb7038fffc3bbf36d7d` | 48632904 |
| PartialDecryptVerifier | `0x7ee28e810086590192e93e6046d90fe4e97f0f60` | 48632904 |
| DecryptCombineVerifier | `0x1fd445c2e5700a0791ca70ac1bc3447b32acc56e` | 48632904 |

Council contract ([davinci-dkg-council](https://github.com/vocdoni/davinci-dkg-council)):

| Contract | Address | Block |
|---|---|---|
| CouncilManager | `0x2f5b110864cbad4017fe8ac59111812278f5f71f` | 48627018 |

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
other applications can share the committee; each registrant gets its own application ids (see
[lifecycle.md](lifecycle.md#how-a-dkg-process-gets-its-key)).

The Council manager runs the development setup of the Council circuits (`circuits-v1`, one
contributor), so Council-mode elections on this beta trust that contributor not to forge dealings.
A production ceremony brings a new manager, and with it a new registry.

### Verifying the deployment

Three checks refuse a registry that pins other programs: the node's startup check,
`davinci_client::organizer::verify_registry` for clients, and davinci-contracts'
`script/verify_deployment.py`, which compares the deployed runtime code with a local build and
reads back every pin. The client and the script also check that the DKG and Council adapters, if
any, point back at the registry; a registry without `councilAdapter()` (R1) reads as having no
Council adapter, and any other failure of that read fails the check. From a davinci-contracts
checkout:

```bash
python3 script/verify_deployment.py --rpc https://rpc.gnosischain.com \
  --registry 0x20b96e465CA7C3536B9C733571ec1eCf42b2eA21 --chain-id 100 \
  --batch-vk  0x6cfc89d562d0b22f04478a5c15b390433eb52f1b03147030b183076260da7a10 \
  --results-vk 0x7bc8c5e9235548386a44b1885732a2a7ffb1badddc8c7fba599d07ece47be794 \
  --root-c 0x05006517b6ccde5da4d890587ba62845b5af8a307c00e87d4b9d05099b16dc80 \
  --ballot-vk-hash 0xbf1e6590bb1ba883d601c4d7d1c6fa2722a78590716874019db6d68fc776bb0e \
  --dkg-manager 0xC6Fb38c42ed3FB35D363a702218746d5C7Da36BF \
  --council-manager 0x2f5b110864cbad4017fe8ac59111812278f5f71f
```

Rebuilding a zkVM program changes its verification key, which needs a new registry. Deploy order:
the DKG contracts (`DeployAll.s.sol` in davinci-dkg) and the Council manager, then the
`ProcessRegistry` (constructor: verifier, verification keys, DKG manager, Council manager, grace
parameters), which creates both adapters.

### Retired registries

Releases up to v0.4.1 follow R1. A node on a later release opens a new database for R2 and leaves
R1's directory in the datadir untouched (see
[configuration.md](configuration.md#data-directory)).

| Registry | ProcessRegistry | Block | DKGManager |
|---|---|---|---|
| R1 | `0x6702e0141B6b72bCF8C1bdff20A82A35C5502E7D` | 48504090 | `0x9999f38ff8bf959e98ddd5d4551f82775219c01b` |

### Chain notes

- Gnosis runs Fusaka: blob transactions carry EIP-7594 cell-proof sidecars. The node detects this
  from `eth_config`, or with the P256VERIFY probe on RPCs that lack it.
- A block takes at most 2 blobs. A batch that needs more is split into several transactions.
- A settlement uses about 400–460k gas and costs almost only its blob fee.
- Some public RPCs serve neither `eth_config` nor `eth_blobBaseFee`; the node falls back to the
  probe, its chain table and `eth_feeHistory`. Public RPCs also rate-limit bursts, so production
  nodes should list their own endpoints first.
