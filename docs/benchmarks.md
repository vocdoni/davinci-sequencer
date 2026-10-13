# Benchmarks

Measured results of the e2e benchmarks ([testing.md](testing.md#benchmarks) shows how to run
them). Proving times for the prover alone are in davinci-zkvm's `BENCHMARK.md`.

## Batch sizes on Gnosis

Measured on 2026-09-27 on Gnosis Chain with one RTX 5090 prover and one node per case
(`--batch-max N`, `--batch-time 2m`, 3 confirmations). Each case is a Merkle-census process with
2N voters: the first batch holds voters 0..N and no refreshes, the steady batch voters N..2N plus
N silent refreshes. Times are seconds from the batch's first submit: "sealed" is when none of its
votes is pending any more, "settled" when all are on-chain. Cost is gas plus blob fee over the
batch's transactions.

| N | nf | batch | refreshes | txs | blobs | gas | cost (xDAI) | sealed s | settled s |
|---:|---:|---|---:|---:|---|---:|---:|---:|---:|
| 4 | 4 | first | 0 | 1 | 1 | 439141 | 0.000131 | 2.0 | 27.1 |
| 4 | 4 | steady | 4 | 1 | 1 | 404318 | 0.000131 | 1.0 | 25.1 |
| 16 | 4 | first | 0 | 1 | 1 | 440346 | 0.000131 | 2.1 | 32.1 |
| 16 | 4 | steady | 16 | 1 | 1 | 405273 | 0.000131 | 1.0 | 21.1 |
| 64 | 4 | first | 0 | 1 | 1 | 438785 | 0.000131 | 2.2 | 30.3 |
| 64 | 4 | steady | 64 | 1 | 1 | 406911 | 0.000131 | 1.2 | 35.4 |
| 128 | 4 | first | 0 | 1 | 1 | 441022 | 0.000131 | 2.3 | 42.7 |
| 128 | 4 | steady | 128 | 1 | 1 | 404609 | 0.000131 | 1.3 | 43.8 |
| 256 | 4 | first | 0 | 1 | 1 | 440084 | 0.000131 | 2.6 | 61.6 |
| 256 | 4 | steady | 256 | 1 | 2 | 460648 | 0.000262 | 1.6 | 64.7 |
| 512 | 4 | first | 0 | 1 | 2 | 495762 | 0.000262 | 3.3 | 104.6 |
| 512 | 4 | steady | 512 | 2 | 2+1 | 864966 | 0.000393 | 124.3 | 160.5 |
| 64 | 16 | first | 0 | 1 | 1 | 439663 | 0.000131 | 2.2 | 32.4 |
| 64 | 16 | steady | 64 | 1 | 2 | 461562 | 0.000262 | 1.2 | 41.4 |

The 512-vote steady batch needed three blobs, more than a Gnosis block takes, so it went out as
two transactions; the remainder waited for the 2-minute batch timer, hence its sealed time.
Settlement cost is almost all blob fee, at the 1 gwei blob base fee floor. The client-side ballot
proofs took about 0.16 s each.

## L1-blob batch sizes on OP Mainnet

Measured on 2026-10-13 (01:53-03:49 UTC) on OP Mainnet in L1-blob mode (registry
`0x8b63fAE96948D69CF92Eb40e4EE0fCE40962267d`, see [deployments.md](deployments.md#op-mainnet))
with one RTX 5090 prover shared with a production node, `e2e/tests/bench_l1.rs`, 3 runs per case,
one node, `--batch-max N`, `--batch-time 2m`. Blobs go to Ethereum at proving start; the
settlement waits for OP's L1 origin to reach the blob block. "Prove" is the prover's GPU time;
"proof -> settled" and "seal -> settled" end at the OP block holding the settlement and start at
the case's first seal. L1 is the blob transaction (21,000 gas plus 131,072 blob gas per blob), OP
the settlement (L2 execution plus the L1 data fee). Prices during the run: Ethereum base fee
0.058-0.121 gwei, blob base fee 0.0033-0.0079 gwei. Rows of every settlement:
[the JSON](superpowers/reports/2026-10-13-l1-blob-da-op-bench.json).

| N | nf | batch | runs | blobs | prove s | proof -> settled s | seal -> settled s | headers | verify gas | L1 µETH | OP µETH | L1 posts |
|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 64 | 2 | first | 3 | 1 | 22.7 | 70.8 | 95.2 | 3.0 | 449516 | 1.96 | 0.039 | 1 |
| 64 | 2 | steady | 3 | 1 | 23.6 | 60.3 | 86.2 | 3.0 | 447190 | 1.98 | 0.040 | 1 |
| 128 | 2 | first | 3 | 1 | 29.0 | 61.6 | 92.1 | 3.0 | 430938 | 1.94 | 0.038 | 1 |
| 128 | 2 | steady | 3 | 1 | 31.4 | 54.1 | 89.6 | 3.0 | 431525 | 1.96 | 0.040 | 1 |
| 256 | 2 | first | 3 | 1 | 43.7 | 42.8 | 91.5 | 3.0 | 430080 | 1.89 | 0.037 | 1 |
| 256 | 2 | steady | 3 | 1 | 48.5 | 39.8 | 89.1 | 3.0 | 429526 | 1.87 | 0.037 | 1 |
| 64 | 16 | first | 3 | 1 | 28.3 | 63.0 | 93.9 | 3.0 | 448029 | 1.90 | 0.038 | 1 |
| 64 | 16 | steady | 3 | 2 | 31.1 | 68.2 | 100.8 | 3.0 | 454186 | 2.40 | 0.040 | 1 |
| 128 | 16 | first | 6 | 1 | 37.9 | 161.9 | 200.7 | 3.0 | 431102 | 2.43 | 0.048 | 2 |
| 128 | 16 | steady | 3 | 3 | 46.1 | 65.5 | 112.8 | 3.0 | 460057 | 3.73 | 0.054 | 3 |
| 256 | 16 | first | 6 | 1 | 49.5 | 156.4 | 196.2 | 3.0 | 440046 | 2.56 | 0.044 | 3 |
| 256 | 16 | steady | 6 | 4 | 63.1 | 141.9 | 208.0 | 3.0 | 438880 | 2.68 | 0.041 | 3 |

Every settlement carried a 3-header L1 proof (430k-460k gas for `verify`, about 800k-860k for the
whole settlement on OP) and landed 85-115 s after its seal: OP's origin lag (about 72 s from the
post to the settlement) dominates, so proofs shorter than that leave `proof -> settled` at 40-70 s
and longer ones would hide it. One post cost 1.9-3.7 µETH on Ethereum (about 0.005-0.01 USD) and
the OP side 0.04-0.05 µETH. At 16 fields, batches of 128 and 256 votes did not go out as one
transaction while the election was open: the seal budget (the Ethereum leg and the proof must fit
the registry's 150 s grace floor) split them into two batches (rows with 6 runs count batches),
and the second waited for the first to land, so the case finished about 200-230 s after its first
seal. A registry for L1-blob mode with a higher grace floor (for example 240 s) would let a 256 x
16 batch out in one transaction.

## Throughput

Measured with `e2e/tests/throughput.rs` on two RTX 5090 provers, each serving one node with two
processes of 2048 voters, at nf = 2 and batches of 512: 8192 votes settled in 678.7 s, 12.07
votes/s overall and 12.45 votes/s in steady state, with both GPUs busy the whole time. The full
report, hardware and image versions are in davinci-zkvm's `BENCHMARK.md`.
