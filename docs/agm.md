# Live meetings

A general assembly wants results a few minutes after the vote closes. The node defaults suit an
election that runs for days; for a meeting, change three things.

- **Node.** Set `DAVINCI_BATCH_TIME=60s` and `DAVINCI_SOLO_WAIT=60s`, so votes settle while voting
  runs and little is left to drain at the end. Cast times become visible to about a minute instead
  of 15, which a room voting within the same few minutes gives away anyway; overwrites stay hidden.
- **Grace.** The organizer calls `setProcessGrace` at creation with the registry's floor
  (`graceFloor`).
- **Closing.** "Voting closes in one minute" is a `setProcessDuration` that moves the end to
  `noticeMin` from now. Every node enters flush mode at once and drains during the notice. Ending
  the process (`setProcessStatus` to ended) closes at once instead.

How the end, the flush and the grace window interact is described in
[lifecycle.md](lifecycle.md#the-end-and-the-grace-window).

## Planning figures

The figures below are for one RTX 5090 prover and a Gnosis-style chain with 2 blobs per
transaction, in steady state (as many refreshes as votes). `nf` is the number of ballot fields.
Transitions of one election serialize on its state root, so the ceiling holds whatever the number
of nodes.

| nf | votes per transaction | round (prove and land) | ceiling (votes/min) |
|---:|---:|---:|---:|
| 2 | ~744 | ~145 s | ~305 |
| 4 | ~430 | ~100 s | ~260 |
| 8 | ~230 | ~75 s | ~185 |
| 16 | ~122 | ~64 s | ~115 |

Before the end, the time budget can cap batches below the transaction capacity: it leaves
`graceFloor − prove_base − settle_margin` for the per-vote cost, which is 75 votes at nf = 16 on
the cold estimate with a 150 s floor and the default margins. The estimate follows the prover's
real cost after a few batches, and past the end the whole grace window counts.

Time from pressing END to results on-chain, with a 150 s grace and a sequencer key:

| Backlog at the end | Last landing | Grace end | Results |
|---|---|---|---|
| none | before the end | end + 150 s | ~2.8–3.0 min |
| one round | end + ~75 s | end + ~225 s | ~4.0 min |
| k rounds (nf = 16) | end + k·~70 s | last landing + 150 s | ~(1.2·k + 2.8) min |

A DKG key adds the committee's decryption round, 1 to 5 minutes, after the request at the grace
end. A shortened end drains during the notice and lands in the first row, about 4.4 minutes from
the announcement. Votes arriving faster than the ceiling leave a backlog at the end whatever the
batch time, so keep large, fast meetings at nf ≤ 8.
