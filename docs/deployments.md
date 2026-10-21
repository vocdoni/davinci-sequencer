# Deployments

Contract addresses and pinned values of the deployments the `--network` presets point at. A
release that moves a preset to a new registry updates this file and `client/src/networks.rs`
together.

## OP Mainnet

Chain id 10, preset `optimism`, L1-blob mode: transitions settle on OP Mainnet and their blobs
live in Ethereum blob transactions. This is the protocol v5.1 pre-production registry, deployed and
verified on 2026-10-21 from davinci-contracts v0.0.54, on the davinci-zkvm v0.5.0 pins and the
pre-production ceremony ballot key of davinci-circom v1.3.0, with the DKG and Council adapters.
Process ids start with `e8d4e286`.

| Contract | Address | Block |
|---|---|---|
| ZiskVerifier | [`0x35Ad4cBc4C77a2Bc69B07e68f2535Da5C6d07AB7`](https://optimistic.etherscan.io/address/0x35Ad4cBc4C77a2Bc69B07e68f2535Da5C6d07AB7#code) (code hash `0xadab570d…cadcc1`, reused) | |
| L1BlobVerifier | [`0x3B95BC14F43ed8Addb83F84D10A949De74eE7529`](https://optimistic.etherscan.io/address/0x3B95BC14F43ed8Addb83F84D10A949De74eE7529#code) (reused from the first OP registry) | 158125135 |
| ProcessRegistry | [`0x02D6c284234B4CE75faB3085cF3f1ba4b7262B97`](https://optimistic.etherscan.io/address/0x02D6c284234B4CE75faB3085cF3f1ba4b7262B97#code) | 158487281 |
| DKG adapter | [`0xffAC853E7B93CE6148b1A551A40aFd88DE517863`](https://optimistic.etherscan.io/address/0xffAC853E7B93CE6148b1A551A40aFd88DE517863#code) | |
| DKGManager | `0x9e799a1d22710056e62c849e3ffe19dd01db2d41` (unchanged) | 158245366 |
| DKGAppManager | `0x1e7f0bec007828e2e0a1a469d9d3e148888fc39a` (unchanged) | |
| Council adapter | [`0xe07BE522a8fE9d1cf85DCA5Eefbe663055D35127`](https://optimistic.etherscan.io/address/0xe07BE522a8fE9d1cf85DCA5Eefbe663055D35127#code) | |
| CouncilManager | `0x281b84dee6305083cc719187e4a2737d61b4396e` (unchanged) | 158245395 |

Pinned values, equal to the SDK release pins (davinci-zkvm v0.5.0):

| Value | Where | Hash |
|---|---|---|
| batch program vk | registry `batchProgramVK` | `0x4b1a527e284af96fa0c3543e9e9b18746a6494edd99c309e58d96a804e3041dc` |
| results program vk | registry `resultsProgramVK` | `0xce964acdd53329fc49ede57f5ea288f044950e136be119287fd0021390689dc4` |
| `rootCVadcopFinal` | registry and verifier | `0x6b2490fba336fb00f0c8719a961de309a8db6f43a9bfb7575ca5f014d609b265` |
| ballot VK hash | registry `ballotVKHash`, state leaf `0x07` | `0x67e8e6b47ea32cf144d6e72886f02b95ed068b157f9d4ccc8e97f335858e4963` |
| verifier code keccak256 | `ZiskVerifier` runtime code | `0xadab570d3e3a4c172d0c75e5aef5d06098b94d70a9c8ff76af239a90fdcadcc1` |

The grace window fits the Ethereum leg: 300 s by default, floor 240 s, ceiling 600 s, at most
1800 s past the end, 60 s shorten notice. The DKG and Council key modes run against the managers
above (their committees and relayer are operated separately).

`L1BlobVerifier` reads OP's L1 origin from the `L1Block` predeploy
`0x4200000000000000000000000000000000000015`. Consumers of OP transitions (SDKs, explorers) read
`ProcessStateTransitionedL1` for the Ethereum blocks and versioned hashes, one block per blob: a
transition above six blobs comes in several Ethereum transactions. There is no `BLOBHASH` on OP.
The preset reads Ethereum from `https://ethereum-rpc.publicnode.com` and `https://eth.drpc.org`:
publicnode refuses request bodies above about 1 MB, dRPC takes the 1.5 MB six-blob sends, and the
node sends a blob transaction to every endpoint. Followers fetch blobs from
`https://ethereum-beacon-api.publicnode.com`, which served full blobs for recent and day-old
slots when the preset was filled in; production nodes should add a beacon node of their own that
keeps full blobs.

## Retired deployments

These registries stay on chain, but no preset follows them. They pin older
program vks, so a node of this release, which proves under protocol v5.1, refuses
them at boot.
Their datadir directories stay untouched next to the current one.

### OP Mainnet, protocol v5 pre-production registry

`ProcessRegistry` `0x6F500A3e3A161EcB3C19A034bFB6a6Ac3b71abE6` (block 158414417), deployed on
2026-10-19 from davinci-contracts v0.0.52 with the DKG adapter
`0x51aB0BB6A455e7f027f3b09b8F1Ea0634806f390` and Council adapter
`0xC2f51d01E16b61d75eAD2e5b88d139739067af17`, followed by the `optimism` preset until the protocol
v5.1 registry replaced it (process ids `3c3e0777…`). It pins the davinci-zkvm v0.4.1 programs
(batch `0x1e4a74a1…d2a8`, results `0xde55d751…e4e9`, rootC as above) and the earlier ballot VK
hash `0x7caffddcd7ea38439cbb45789b810d8eafceca65cff93dbeaae409b2bf64e219`; its record is
davinci-contracts' `deployments/10-0x6f500a3e3a161ecb3c19a034bfb6a6ac3b71abe6.json`. A node of this release refuses it at boot.

### OP Mainnet, ZisK 1.3.2 registry with adapters

`ProcessRegistry` `0x64d41aD49009204E8dAcc153Fc98a5Ee14cB6916` (block 158245426), deployed on
2026-10-15 from davinci-contracts `338ac77` with the DKG adapter
`0x0c1E7120FaBf95fDe294839A78780334d27cfCff` and Council adapter
`0xa5A1e4B204976709EB545Af0189211c41e20193d`, followed by the `optimism` preset until the protocol
v5 registry replaced it. It pins the ZisK 1.3.2 programs (batch `0xe39e4600…988b`, results
`0x39469455…d167`, rootC as above) and the earlier ballot VK hash
`0xbf1e6590bb1ba883d601c4d7d1c6fa2722a78590716874019db6d68fc776bb0e`; its record is
davinci-contracts' `deployments/10.json` before v0.0.53. A node of this release refuses it at
boot.

### OP Mainnet, ZisK 1.3.2 registry without adapters

`ProcessRegistry` `0x7bfa68879f118C98B864FB52d2aD3a112eF1558F` (block 158164175), deployed on
2026-10-13 from davinci-contracts `fd79251` on the same `ZiskVerifier` and `L1BlobVerifier`, and
followed by the `optimism` preset until the registry with the DKG and Council adapters replaced
it. Same pins and grace window as the ZisK 1.3.2 registry above, but no DKG or Council adapter. Its record
is davinci-contracts' `deployments/10-0x7bfa.json`.

### OP Mainnet, ZisK 1.3.0 registry

`ProcessRegistry` `0xC5f88160a1Df51A3Ee56b970b12B8308799c05A7` (block 158155408), deployed on
2026-10-13 from davinci-contracts `44fdca1` on the reused `ZiskVerifier`
`0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c` and `L1BlobVerifier`, followed by the `optimism`
preset until the ZisK 1.3.2 registry replaced it the same day. It pins ZisK 1.3.0 and the v0.2.0
vks (batch `0xa0ad5c4c…35fa`, results `0xe6935894…8f68`, rootC `0x05006517…dc80`). Its record is
davinci-contracts' `deployments/10-0xC5f8.json`.

### OP Mainnet, first registry

`ProcessRegistry` `0x8b63fAE96948D69CF92Eb40e4EE0fCE40962267d` (block 158125136), deployed on
2026-10-12 from davinci-contracts `e7de916` on the same `ZiskVerifier` and `L1BlobVerifier`,
followed by the `optimism` preset of release v0.5.x builds only. Its grace floor (150 s) left too
little room for the Ethereum leg, and it pins the v0.1 vks.

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

For the current OP registry, from davinci-contracts at `v0.0.54` after `forge build`:

```bash
python3 script/verify_deployment.py --rpc "$RPC" --chain-id 10 \
    --registry 0x02D6c284234B4CE75faB3085cF3f1ba4b7262B97 \
    --verifier 0x35Ad4cBc4C77a2Bc69B07e68f2535Da5C6d07AB7 \
    --batch-vk 0x4b1a527e284af96fa0c3543e9e9b18746a6494edd99c309e58d96a804e3041dc \
    --results-vk 0xce964acdd53329fc49ede57f5ea288f044950e136be119287fd0021390689dc4 \
    --root-c 0x6b2490fba336fb00f0c8719a961de309a8db6f43a9bfb7575ca5f014d609b265 \
    --ballot-vk-hash 0x67e8e6b47ea32cf144d6e72886f02b95ed068b157f9d4ccc8e97f335858e4963 \
    --dkg-manager 0x9e799a1d22710056e62c849e3ffe19dd01db2d41 \
    --council-manager 0x281b84dee6305083cc719187e4a2737d61b4396e \
    --l1-block 0x4200000000000000000000000000000000000015 --grace 300,240,600,1800,60 \
    --l1-blob-verifier 0x3B95BC14F43ed8Addb83F84D10A949De74eE7529
```
