# L1BlobVerifier live check on OP Mainnet

Verifier: `0xa6bE72b2e1AEF3aBD5DA031D4C893C572A01a67e` (constructor arg: L1Block predeploy
`0x4200000000000000000000000000000000000015`).
Deploy tx: `0x1ecfb7c7aab299b4a587d8d20fb9607a3418fb18e2a22228121f3fca18e4d4ce`, gas used 1,519,178,
effective gas price 11,320 wei, L2 execution cost 17,197,094,960 wei (the L1 data fee is extra).

Each proof was built once with top = the Ethereum head (26178887), checked with `verify_local`
against OP's `L1Block.hash()`, then by `eth_call` of `verify` on OP (hashes and blocks equal to
`verify_local`'s), and checked again 15 s later against the new origin.

| Tx | L1 block | Index | Headers | Calldata bytes | estimateGas | Origin, check 1 | Origin, check 2 |
|---|---|---|---|---|---|---|---|
| `0x06ac4266...ab2dace` | 26178881 | 227 | 7 | 6948 | 641,505 | 26178881 | 26178882 |
| `0x091f28eb...12a8f0` | 26178881 | 90 | 7 | 7428 | 571,288 | 26178882 | 26178884 |
| `0x1c103a61...c775d3` | 26178879 | 229 | 9 | 13700 | 834,706 | 26178884 | 26178885 |

The transactions carry 6 or more, 3 and 1 blobs respectively. The repeat estimateGas values differ by
under 50 gas.

Conclusion: the verifier accepts proofs of real Ethereum blob transactions on OP Mainnet against
the live L1Block origin, and returns exactly the versioned hashes and blocks the Rust builder
computed. The same proof stays valid when the origin advances by one to three blocks, because the
header chain reaches the current Ethereum head and the origin only lags it. Verification costs
roughly 570k to 835k gas, dominated by header count and trie proof size rather than blob count, so
the calldata (7 to 14 KB) is the part to watch on OP.
