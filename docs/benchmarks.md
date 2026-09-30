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

## Throughput

Measured with `e2e/tests/throughput.rs` on two RTX 5090 provers, each serving one node with two
processes of 2048 voters, at nf = 2 and batches of 512: 8192 votes settled in 678.7 s, 12.07
votes/s overall and 12.45 votes/s in steady state, with both GPUs busy the whole time. The full
report, hardware and image versions are in davinci-zkvm's `BENCHMARK.md`.
