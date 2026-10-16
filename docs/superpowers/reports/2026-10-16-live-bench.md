# Live OP Mainnet benchmark campaign report, 2026-10-16

Campaigns c1 (wave 1) and c2 (waves 2 and 3) on OP Mainnet with Ethereum blobs for data availability (L1-blob
mode), plus five TypeScript SDK runs. Every number below comes from the campaign files under the bench cache
(`summary.md` / `summary.json`, `runs/*`, `budget.json`, `plan.json`, the run logs and ledgers) or from the SDD
progress ledgers. Where two sources disagree, or a source is missing, the text says so. Process ids and hashes are
abbreviated.

## 1. Result in brief

- Across the 18 campaign scenarios that ran live (4 in c1, 14 in c2) and the S-8 recovery, no vote was lost
  (`no_vote_lost` yes everywhere). `clean` and `counters_ok` are yes everywhere. `tally_ok` and `results_ok` are yes
  everywhere except w2/X3, which the budget stopped and the bench canceled on purpose (section 8).
- Bisections, isolated votes, held batches, `refresh_overflow`, `guest_rejection` and `finalize_failed` are 0 on both
  nodes in every scenario of both campaigns. Unexpected errors and stale errors: none.
- Measured ceiling (c1, steady votes/min): nf 2 reaches 298 / 474 / 668 at batch 128 / 256 / 512; nf 16 reaches 320 /
  369 / 308. The 240 s floor bound no cell.
- Forced races: 20 of 20 recovered in X1 and X2 (detection 0.2-4.0 s, recovery 2.8-4.4 s). All eight chaos scenarios
  recovered without a lost vote.
- The production TS run prod-a-1500 (1500 voters, nf 16) settled 1500 of 1500, results equal the tally, 61.4 votes/min.

## 2. Setup

| Item | Value |
|---|---|
| Node, c1 wave 1 | 5168102 at the wave start (R, S-2, S-8); 0e52343 for S-16 and S-c (after the memory fix, section 9.1) |
| Node, c2 | d1c534c (the same actor code as release v0.6.0; the public history later rewrote it to 9d23e9b, and v0.6.0 is e169770) |
| Bench (davinci-bench) | c1: 298a06a at the wave start, then e634ea0 / db96cbc for the S-8 recovery, b1e0dbb for S-16 and S-c. c2: ab41d4c for waves 2 and 3. Branch head now 00bb194 (local, rebased) |
| SDK bench | davinci-sdk-bench a909888 (smoke1, SDK 3.x base); 200ef27 on SDK 4.0.1 (RPC failover list) for the later production runs. prod-a-500 ran on the SDK 3.x base (section 9.6) |
| Campaign registry (c1, c2, smoke1) | 0x7bfa...1558F on OP Mainnet (chain 10), verifier 0x35Ad...7AB7, pins checked at every start |
| L1BlobVerifier | 0x3B95...7529 on the OP L1Block predeploy |
| Production registry (TS production runs) | 0x64d41a...6916, one production node, prover z6, start block 158245426 |
| Provers | z6 and z7 (GPU), ZisK 1.3.2-alpha, `ZISK_MINIMAL_MEMORY_FROM=256` on both hosts (2048 OOM-killed nf-16 batches) |
| Pins (c2 `pins.json`) | batch program VK 0xe39e...988b, results program VK 0x3946...d167, root C 0x6b24...b265, ballot VK hash 0xbf1e...bb0e |
| Topology | two competing sequencers A and B (one per prover) plus an observer O; 3 nodes at ports 9101-9103 behind per-upstream loopback proxies |
| Grace | the dry scripts use 300,240,600,1800,60; C7 ran with a 600 s grace (section 9.3) |

Fee conditions. Wave 1 ran at base fees of about 0.07 gwei and blob fees of about 0.004 gwei. In c2 the fee gate
(ceilings 0.5 gwei execution, 0.02 gwei blob) said NO-GO from about 14:00 UTC on 2026-10-15 (14:47 check: base
0.657 gwei, blob 0.0417 gwei). The owner asked for wave 2 to run anyway, on purpose, at high Ethereum fees, and
the gate ceilings were raised through `DAVINCI_BENCH_MAX_{EXEC,BLOB}_WEI` (bench ab41d4c, commit 00bb194 keeps the
override). Per the run logs' progress lines, wave 2 first block fees were:

| Scenario | base fee (gwei, min-max) | blob fee (gwei, min-max) |
|---|---|---|
| X1 | 0.726-1.443 | 0.041-0.093 |
| X2-a | 0.624-1.088 | 0.038-0.067 |
| X2-b | 0.586-1.032 | 0.037-0.066 |
| X5 | 0.382-0.93 | 0.022-0.058 |
| X4 (run later, 18:42) | 0.109-0.145 | 0.006-0.009 |
| X3 (run later, stopped) | about 0.098 | about 0.005-0.006 |

The ledger gives 0.38-0.94 gwei for wave 2; the log lines above reach 1.443 gwei in X1 (the ledger range and the
logs differ; the logs are the finer source). Wave 3 ran at base 0.054-0.172 gwei (C5), except C4 which saw up to 0.356 gwei
(blob 0.003-0.019 gwei).

Timeline: c1 wave 1 2026-10-14 from 00:59 UTC (S-8 recovery and the S-16/S-c resume the same day); c2 wave 2 started
(X1 t0 14:52:51 UTC on 2026-10-15); X3 t0 18:26:51 (report 18:33:26); X4 t0 18:42:52 (finished 18:58:51); wave 3
C1 t0 18:59:07 and C8 t0 22:46:09; the campaign ended 23:16 UTC.

## 3. Throughput and the ceiling table

Wave 1 cells (c1; S-2 and S-16 are 4 elections x 3072 voters; steady votes/min, mean batch in brackets). The `floor`
flag (240 s floor binding) is false in every cell, so no cell was bounded by it, and at nf 16 the open-window batches
reached the full B:

| nf \ B | 128 | 256 | 512 |
|---|---|---|---|
| 2 | 298 (mean batch 128) | 474 (mean batch 256) | 668 (mean batch 512) |
| 16 | 320 (mean batch 128) | 369 (mean batch 256) | 308 (mean batch 512) |

Notes. The nf 16 / B 512 cell (308) is below B 256 (369); the sources do not explain it. S-c (nf 16, B 256, 2
elections) has its own row: steady 203 votes/min. nf 8 (S-8) has no cell: the run was interrupted (section 9.1). In c2 the
`cells` fields are empty (waves 2 and 3 are race and chaos scenarios, not ceiling cells).

Per-scenario throughput. "Votes" is the count of votes settled, from the run logs' "votes in N transitions" lines
(the reports' `accepted` field counts 1280 for X1 and 1200 for X4, twice the settled votes; I read that as both routes
receiving each vote, which the reports do not say). Lost races are per node, from the summary.

Wave 1 (c1):

| scenario | votes | transitions | overall votes/min | steady votes/min | lost races A | lost races B |
|---|---|---|---|---|---|---|
| w1/R | 5805 | 34 | 86.5 | 90.9 | 6 | 10 |
| w1/S-16 | 12288 | 44 | 296.0 | 315.0 | 0 | 0 |
| w1/S-2 | 12288 | 44 | 475.4 | 531.0 | 0 | 0 |
| w1/S-c | 1536 | 6 | 161.6 | 203.2 | 2 | 4 |

S-8 (4 elections S8-1..4, interrupted by the OOM) is not in the summary table. Its `audit.json` counts 6656
settled and 64 never admitted; 5568 planned votes were never sent (1472 + 1536 + 1536 + 1024). The S-8 `progress.log`
ends at 6144 sent / 4096 settled, which differs from the audit's 6656 settled; the audit is taken as the final count.

Waves 2 and 3 (c2):

| scenario | votes | transitions | overall votes/min | steady votes/min | lost races A | lost races B |
|---|---|---|---|---|---|---|
| w2/X1 | 640 | 10 | 34.5 | 33.8 | 5 | 5 |
| w2/X2-a | 320 | 5 | 41.6 | 40.0 | 0 | 5 |
| w2/X2-b | 320 | 5 | 39.7 | 39.0 | 5 | 0 |
| w2/X3 | 64 | 1 | 45.3 | n/a | 1 | 0 |
| w2/X4 | 600 | 3 | 56.4 | 50.8 | 2 | 1 |
| w2/X5 | 400 | 2 | 56.0 | 120.0 | 0 | 1 |
| w3/C1 | 600 | 8 | 24.4 | 28.8 | 3 | 0 |
| w3/C2 | 600 | 8 | 24.0 | 28.7 | 0 | 0 |
| w3/C3 | 450 | 7 | 21.6 | 27.0 | 0 | 1 |
| w3/C4 | 600 | 7 | 23.1 | 27.6 | 0 | 2 |
| w3/C5 | 750 | 11 | 24.0 | 26.7 | 0 | 1 |
| w3/C6 | 600 | 7 | 25.4 | 30.2 | 0 | 2 |
| w3/C7 | 750 | 9 | 23.3 | 26.1 | 1 | 3 |
| w3/C8 | 1300 | 6 | 67.1 | 93.2 | 2 | 2 |

Reading the c2 numbers: X1 to X5 and C1 to C8 are not throughput probes. The batch timer (15 min in wave 2, 5 min in
wave 3) and forced seals set the rate. X5's "steady 120.0" comes from 2 transitions and is not a steady state. C8
(backlog of 1300 votes in 6 transitions) is the only wave 3 scenario near a rate: 67.1 overall, 93.2 steady.

## 4. Latency and stage tables

Seconds; each cell is p50 / p90 / p99 / max. Stages are the bench's `queued`, `proving`, `l1`, `op` and `total`
(the summary does not define them; my reading is queue wait, prover job, L1 post, OP settle, whole path).

Wave 1 (c1):

| scenario | queued p50/p90/p99/max | proving p50/p90/p99/max | L1 post p50/p90/p99/max | OP settle p50/p90/p99/max | total p50/p90/p99/max |
|---|---|---|---|---|---|
| w1/R | 378.4 / 860.4 / 1180.0 / 1534.7 | 78.7 / 115.5 / 128.9 / 128.9 | 2.9 / 41.9 / 45.4 / 63.4 | 4.6 / 51.2 / 86.0 / 90.0 | 491.7 / 966.4 / 1270.1 / 1608.2 |
| w1/S-16 | 262.3 / 524.4 / 529.4 / 529.5 | 92.0 / 181.0 / 367.3 / 367.3 | 0.0 / 25.0 / 73.9 / 73.9 | 4.3 / 36.4 / 71.6 / 71.6 | 358.1 / 710.3 / 896.2 / 896.3 |
| w1/S-2 | 152.9 / 266.6 / 267.1 / 267.2 | 55.8 / 93.7 / 95.4 / 95.4 | 0.0 / 32.4 / 100.9 / 100.9 | 7.1 / 61.4 / 84.0 / 84.0 | 263.6 / 362.1 / 364.2 / 364.2 |
| w1/S-c | 144.7 / 278.5 / 278.5 / 278.5 | 86.6 / 88.6 / 88.6 / 88.6 | 21.6 / 56.0 / 56.0 / 56.0 | 11.1 / 57.3 / 57.3 / 57.3 | 244.7 / 372.1 / 372.2 / 372.2 |

Waves 2 and 3 (c2):

| scenario | queued p50/p90/p99/max | proving p50/p90/p99/max | L1 post p50/p90/p99/max | OP settle p50/p90/p99/max | total p50/p90/p99/max |
|---|---|---|---|---|---|
| w2/X1 | 0.3 / 0.6 / 1.6 / 1.6 | 23.5 / 26.5 / 26.6 / 26.6 | 4.8 / 13.9 / 144.5 / 144.5 | 64.0 / 78.4 / 86.0 / 86.0 | 91.3 / 102.3 / 231.1 / 231.1 |
| w2/X2-a | 0.3 / 0.4 / 0.4 / 0.4 | 24.6 / 24.9 / 24.9 / 24.9 | 9.1 / 11.6 / 11.6 / 11.6 | 62.5 / 66.6 / 66.6 / 66.6 | 87.4 / 91.9 / 91.9 / 91.9 |
| w2/X2-b | 0.6 / 0.6 / 0.6 / 0.6 | 26.8 / 26.9 / 26.9 / 26.9 | 8.0 / 22.0 / 22.0 / 22.0 | 62.0 / 75.1 / 75.1 / 75.1 | 88.6 / 102.6 / 102.6 / 102.6 |
| w2/X3 | 1.5 / 1.5 / 1.5 / 1.5 | 25.4 / 25.4 / 25.4 / 25.4 | 5.3 / 5.3 / 5.3 / 5.3 | 57.9 / 57.9 / 57.9 / 57.9 | 84.8 / 84.8 / 84.8 / 84.8 |
| w2/X4 | 115.6 / 174.3 / 187.8 / 190.1 | 85.7 / 86.8 / 86.8 / 86.8 | 8.6 / 10.7 / 10.7 / 10.7 | 4.0 / 4.0 / 4.0 / 61.2 | 205.3 / 263.8 / 277.3 / 278.8 |
| w2/X5 | 186.0 / 292.9 / 327.9 / 333.7 | 50.7 / 51.3 / 51.3 / 51.3 | 5.7 / 10.8 / 10.8 / 10.8 | 35.0 / 43.5 / 43.5 / 43.5 | 274.6 / 387.6 / 422.6 / 428.4 |
| w3/C1 | 181.0 / 314.6 / 385.5 / 403.0 | 34.8 / 40.4 / 40.4 / 40.4 | 4.9 / 6.8 / 10.8 / 10.8 | 49.1 / 57.6 / 67.7 / 67.7 | 267.6 / 400.1 / 466.4 / 483.9 |
| w3/C2 | 176.2 / 303.7 / 406.3 / 453.2 | 30.3 / 41.1 / 41.1 / 41.1 | 4.1 / 23.2 / 23.2 / 23.2 | 51.1 / 78.2 / 78.2 / 78.2 | 262.4 / 389.6 / 488.9 / 535.9 |
| w3/C3 | 172.6 / 286.9 / 386.0 / 409.9 | 29.4 / 40.0 / 40.0 / 40.0 | 4.9 / 6.2 / 6.2 / 6.2 | 51.3 / 62.3 / 66.5 / 66.5 | 259.1 / 374.6 / 478.8 / 502.8 |
| w3/C4 | 214.7 / 590.6 / 817.7 / 851.8 | 34.0 / 48.4 / 48.4 / 48.4 | 12.4 / 60.7 / 60.7 / 60.7 | 65.2 / 80.0 / 80.0 / 80.0 | 312.8 / 729.4 / 956.5 / 990.6 |
| w3/C5 | 210.5 / 373.8 / 474.4 / 492.2 | 31.1 / 41.4 / 41.4 / 41.4 | 2.4 / 7.4 / 11.2 / 11.2 | 50.8 / 64.8 / 68.7 / 68.7 | 280.0 / 415.6 / 518.9 / 536.7 |
| w3/C6 | 203.2 / 485.7 / 677.4 / 697.1 | 32.0 / 51.8 / 51.8 / 51.8 | 5.4 / 9.4 / 13.5 / 13.5 | 51.0 / 61.9 / 64.9 / 64.9 | 286.9 / 570.3 / 762.0 / 781.7 |
| w3/C7 | 211.6 / 719.2 / 1075.2 / 1088.6 | 30.7 / 71.8 / 71.8 / 71.8 | 4.0 / 15.4 / 15.4 / 15.4 | 57.0 / 64.7 / 65.6 / 65.6 | 299.3 / 805.3 / 1161.3 / 1174.7 |
| w3/C8 | 239.3 / 416.3 / 416.3 / 449.7 | 126.6 / 132.5 / 132.5 / 132.5 | 0.0 / 10.5 / 11.2 / 11.2 | 4.2 / 60.4 / 64.0 / 64.0 | 338.4 / 545.2 / 545.2 / 578.6 |

Observations. Proving p50: 23.5-26.8 s in X1 / X2 (batch_max 64), 50.7 s in X5, 85.7 s in X4, 29.4-34.8 s in C1-C7
and 126.6 s in C8 (batch_max 512 in X4, X5, C1-C8). The `op` p50 is 35.0-65.2 s in most c2 rows but 4.0 s in X4,
4.2 s in C8 and 4.3-11.1 s in the c1 rows; I did not split this further.

## 5. Races

c2 forced races (X1 revert path, X2 event path, X3 skew, X4 and X5 organic):

| scenario | rounds | path | winners (a/b) | detect | recover | wasted prover job |
|---|---|---|---|---|---|---|
| w2/X1 | 10 | revert | 5/5 | 0.2-1.0 s | 2.9-4.4 s | 23.3-27.2 s |
| w2/X2-a | 5 | event | 5/0 | 3.9-4.0 s | 4.2-4.3 s | n/a |
| w2/X2-b | 5 | event | 0/5 | 2.5-2.5 s | 2.8-3.1 s | n/a |

- X1: 10 rounds, 10 of 10 races, winners 5 / 5. Every round took the revert path (the bench's path label). Detect 0.2-1.0 s, recover 2.9-4.4 s. The loser's
  wasted prover job cost 23.3-27.2 s. Each round shows one post, at 21.99-38.50 µETH.
- X2-a: prover B held 150 s on 5 /prove calls (5 dropped); A won all 5 rounds by the event path, detect 3.9-4.0 s,
  recover 4.2-4.3 s, L1 post 22.10-31.76 µETH. X2-b is the mirror (prover A held): B won 5/5, detect 2.5 s, recover
  2.8-3.1 s, 17.20-23.14 µETH.
- X3 was meant to run 5 skewed rounds (A held 150 s, lag). It ran 1 round (0 of 5 races: B settled with no race, path
  none) and was stopped by the organizer budget (section 8). Not rerun live (the bench refuses a rerun once ballots
  are spent); the race dry X3 had 2 of 2 on the revert path.
- X4 (two elections, batch_max 512) and X5: organic races, lost: X4 A 2, B 1; X5 B 1. All recovered, no vote lost.
- Wave 3 organic lost races (per node): C1 A 3; C3 B 1; C4 B 2; C5 B 1; C6 B 2; C7 A 1, B 3; C8 A 2, B 2; C2 none.
- Wave 1 (c1): R lost races A 6, B 10; S-c A 2, B 4; S-2 and S-16 none. All recovered, no vote errored. The ledger
  says "12 lost races recovered" for R while the summary sums to 16; the summary (from the JSON) is quoted.

## 6. Chaos

Wave 3 (c2). Times are seconds from the scenario start; `recovery` is the bench's time from undo to the next landing.

| scenario | fault | applied | undone | recovery | effects |
|---|---|---|---|---|---|
| w3/C1 | kill A with its blob post pending | 338 s | 369 s | 37 s | post x1, post on record x1 |
| w3/C2 | kill B after a seal | 326 s | 447 s | 7 s | none |
| w3/C3 | kill the key holder in the grace window | 1327 s | 1508 s | n/a | none |
| w3/C4 | prover A down | 241 s | 841 s | 155 s | post x2, transient failure x8 |
| w3/C5 | prover B slow | 181 s | 1381 s | 88 s | post x5 |
| w3/C6 | A's OP RPC answers 503 | 301 s | 601 s | 97 s | RPC failover x23, post x1 |
| w3/C6 | A's OP RPC down | 600 s | 901 s | 83 s | RPC failover x13, post x2 |
| w3/C6 | B's Ethereum read RPC down | 301 s | 601 s | 55 s | none |
| w3/C6 | O's first beacon down | 301 s | 601 s | 97 s | beacon failover x1 |
| w3/C7 | b fee cap (first) | 181 s | 1084 s | 96 s | post x1 |
| w3/C7 | a fee cap | 1629 s | 1752 s | 101 s | post x1, post on record x1 |
| w3/C7 | b fee cap | 1629 s | 1755 s | 97 s | post x1, post on record x1 |

- C1 and C2 killed a sequencer (A with a blob post pending, B after a seal); both restarted and the post was found on
  record (C1: recovery 37 s, no double post). C2 recovery 7 s.
- C3 killed the key holder in the grace window (applied at 1327 s, undone 1508 s); the results appeared after the
  restart (audit `results_ok` yes). No recovery time is defined for it.
- C4 (prover A down 600 s): 2 posts, 8 transient failures, recovery 155 s. C5 (prover B slow): recovery 88 s.
- C6: A's OP RPC answering 503 (23 failovers) and down (13 failovers); O's first beacon down (1 beacon failover);
  B's Ethereum read RPC down had no other entry to fail over to (the run log says so), so it shows no effect and a
  55 s recovery. The beacon failover is between two proxies of the same provider (the gate found no second free
  beacon with full blobs), so provider redundancy was not exercised.
- C7: three faults. B's L1 fee cap alone (181 s to 1084 s): one post, recovery 96 s. Then both sequencers' caps at
  1629 s (undone at 1752 s for A and 1755 s for B): one post each, "post on record" once each, recovery 101 s and
  97 s. `summary.json` keys chaos by name, so it dropped the first "b fee cap" row (the table above adds it back from
  `w3-all.log`).
- C8: 1300-vote backlog, no fault, 6 transitions.

Chaos dry runs (anvil, scale 0.2, node bfa8126, bench fea601d; section 7) fired the same scenarios: C6 fired three
faults (B's Ethereum read RPC down, A's OP RPC 503 and down), and C7 fired B capped from 180 s to 1081 s plus both capped at 1141 s.

## 7. Dry runs (anvil, scale 0.05-0.3)

Run before each live wave on two anvils with both GPU provers. Every audit in the dry runs is `no_vote_lost clean
tally_ok results_ok counters_ok` with findings none.

| Dry | Node / bench | Scale | Result |
|---|---|---|---|
| dry-w1b (R, S-16) | node not recorded (5168102 was current) / bench 9bd7306 (ledger, Task 13) | 0.05 (ledger) | R 285 votes / 14 transitions, 29.1 votes/min (28.7 steady); S-16 632 / 44, 52.2 (57.9 steady) |
| dry-w2 (X1, X2-a, X3, X5) | 0e52343 / b1e0dbb | 0.3 | X1 60 votes / 3 transitions, 3 of 3 races (event path); X2-a 40 / 2, 2 of 2; X3 40 / 2, 2 of 2 on the revert path; X5 120 / 2 |
| dry-w3 (C1-C8) | bfa8126 / fea601d | 0.2 | C1 120 votes / 2 transitions; C2 120 / 2; C3 90 / 2; C4 120 / 2; C5 150 / 2; C6 120 / 2; C7 150 / 2; C8 260 / 2 |

dry-w1b is the Task 13 41-minute dry (plan created 2026-10-13 23:01 UTC); its numbers match the ledger (R 29.1,
S-16 52.2 / 57.9, 113 at B 512), which adds seal to settle p50 about 21 s and 7 lost races at R ends (B 6, A 1).
dry-w1 (an earlier attempt) ended with `stopped by SIGTERM; rig down`, exit 1. The
remaining `dry-*` directories are plan, cancel and stop checks without a campaign summary.

## 8. Audits

Verdicts per scenario from the campaign summaries. The per-election `audit` lines in the run logs agree where I
checked them (X3, X4, S-8). `observer incomplete` is no everywhere.

c1:

| scenario | no_vote_lost | clean | tally_ok | results_ok | counters_ok | observer incomplete |
|---|---|---|---|---|---|---|
| w1/R | yes | yes | yes | yes | yes | no |
| w1/S-16 | yes | yes | yes | yes | yes | no |
| w1/S-2 | yes | yes | yes | yes | yes | no |
| w1/S-c | yes | yes | yes | yes | yes | no |

c2:

| scenario | no_vote_lost | clean | tally_ok | results_ok | counters_ok | observer incomplete |
|---|---|---|---|---|---|---|
| w2/X1 | yes | yes | yes | yes | yes | no |
| w2/X2-a | yes | yes | yes | yes | yes | no |
| w2/X2-b | yes | yes | yes | yes | yes | no |
| w2/X3 | yes | yes | NO | NO | yes | no |
| w2/X4 | yes | yes | yes | yes | yes | no |
| w2/X5 | yes | yes | yes | yes | yes | no |
| w3/C1 | yes | yes | yes | yes | yes | no |
| w3/C2 | yes | yes | yes | yes | yes | no |
| w3/C3 | yes | yes | yes | yes | yes | no |
| w3/C4 | yes | yes | yes | yes | yes | no |
| w3/C5 | yes | yes | yes | yes | yes | no |
| w3/C6 | yes | yes | yes | yes | yes | no |
| w3/C7 | yes | yes | yes | yes | yes | no |
| w3/C8 | yes | yes | yes | yes | yes | no |

- w2/X3: `tally_ok` NO and `results_ok` NO because the scenario was stopped by the organizer budget and the bench
  canceled X3 before results ("canceled before results; none expected"); `report.md` says sent 128, accepted 128, unsent 576 (both routes);
  `progress.log` and the run log give 64 accepted votes in 1 transition. The plan was 320 ballots (run log) on a 640-voter census (`plan.json`, the ledger's "64/640"); `no_vote_lost` and `clean` yes.
- c1 S-8 (not in the table): the interrupted run's own audit (08:29 UTC) gave `no_vote_lost` yes, `clean` yes,
  `counters_ok` yes, `tally_ok` and `results_ok` NO (no results yet; the audit returned before the grace ended, a
  bench bug fixed in db96cbc). The key holders published after the grace (08:34) and the results equal the audit's
  expected tallies for S8-1..4 exactly.
- c1 R: 76 refusals with code 41201, all revotes sent after R2's early end (33 on A, 43 on B). They are correct node
  refusals; first ballots count; the audit marks them `NeverAdmitted` (allowed).

## 9. Findings by kind

Vote loss counters, unexpected errors, stale errors: all zero or empty in both campaigns. Failed or missing results:
only X3 (canceled) and S-8 before the grace (section 8). Votes pending after the wait: none (every settled count
equals the audited sent count, except the cancelled X3 and the interrupted S-8).

### 9.1 Node memory (c1, fixed)
Wave 1's run scope (MemoryMax 12G) was OOM-killed at 02:58:04 UTC on 2026-10-14 after about 2 h: node A 8.14 GB, node
B 7.51 GB, observer 2.33 GB, bench 3.39 GB RSS. Root cause (memory investigation): about 80% glibc malloc arena
retention after large per-batch temporaries (13-33 MB JSON bodies, about 1500 redb commits) on about 50 threads, plus
the redb default 1 GiB cache. Fixes: e83576b (`--db-cache-mb`, 256 MiB), 936bf4a (release arbo read snapshots after
every message), b136f14 (Arc ProveRequest, boot scans only in-flight votes), 5afd408 (jemalloc and a gated MEMSOAK
soak: peak RSS 3002 MB before and 692 MB after in the 40 x 512 nf 16 soak). The restarted rig (unit with
MemoryMax 40G) recovered S-8; its RSS log peaked at 818 MB (A) and 940 MB (B) (the ledger does not say which binary).
On the fixed node 0e52343 the S-16 / S-c resume peaked at 612 MB (A) and 592 MB (B).

### 9.2 Fee-capped L1 post retry (node, fixed)
Found in the chaos dry C7 (node bfa8126): a fee-capped L1 post retried every second with one WARN per try (802
retries in 13.5 minutes in the dry's log). Fix 2cbad79 ("back off fee-capped l1 posts and log the run once"; the
finding asked for one L1 block minimum, exponential to about 60 s). Later commits hold the L1 post while the OP settlement would pass its fee
cap (9d664dd, 137090b, 762bce9). In c2's C7 (node d1c534c) B recovered in 96 s from a 903 s cap, and both recovered from the end cap in 97-101 s.

### 9.3 C7 grace ceiling (bench, fixed)
The first chaos launch was refused at plan: C7 asked for a 900 s grace but the registry accepts 240..=600 s. Fix
fea601d (900 to 600) with a grace-range assertion in the scenario consistency test (a RED test on 900, 90 bench
tests green). The c2 chaos ran with 600 s.

### 9.4 Organizer cap stop of X3 (bench / budget)
In c2's first wave 2 launch X3 and X4 were skipped by the budget (per-key L1 cap 620 µETH). After the owner raised the
cap and funded Ethereum (inflows on seq-a.l1 and seq-b.l1), X3 started at 18:26:51 UTC and was stopped by the
organizer.op budget: spent 500055883387817 wei against a cap of 250000000000000 wei. The spend was the owner's
0.5 mETH transfer to the deployer for the OP DKG and registry deploys, counted as campaign spend. `w2-X3/report.md` says sent 128, accepted 128, unsent 576 (both routes); the 64 comes from `progress.log` ("sent 128/320 acc 64") and the log's "64 votes in 1 transitions". Plan size: the run log says 320 ballots; `plan.json` gives X3 640 voters (census size, as X1 has 1280 voters for 640 ballots), and the ledger's "64/640" counts census voters. w3/C1 stopped at its start with nothing sent. Fix: the organizer cap went from 0.00025 to 0.0008 ETH,
X4 (t0 18:42:52) and wave 3 (C1 t0 18:59:07) were relaunched. X3 was not rerun (covered by the race dry).

### 9.5 Bench audit results wait (bench, fixed)
`audit --run` returned inside the 75 s notice after a shortened end because the registry still read READY until the
new end; that took the rig down before the key holders could publish. Fix db96cbc (waits from the registry's view).

### 9.6 TS bench against the new registry (TS bench, fixed)
prod-a-500 settled 550 of 550 with no error, but the bench's own results wait (SDK 3.x base) threw an empty error
before the grace ended against the new registry with adapters. The report shows "match false"; the results read on
chain with the SDK v4 equal the expected tally [66,64,65,58,65,63,61,58] (ledger). The bench was rebased onto SDK
4.0.1; prod-a-1500 shows "results = tally".

### 9.7 Private RPC outage, 17:05-17:35 UTC, 2026-10-16
The owner's private OP RPC had a connect outage of about 30 minutes. `prod-v4-smoke` (17:05:58 UTC) died at start with
"cannot read the optimism ProcessRegistry ... request timeout"; `prod-v4-smoke2` (17:10) settled 3 of 3 with no error but
its results wait ended with "request timeout (code=TIMEOUT)" and the report shows `match false` (expected [1,2], 3
voters). The production node failed over to a public endpoint once at 17:11 UTC, as designed; the bench used only the
private RPC. Bench 200ef27 takes a failover list (the SDK's failover provider); `ts-prod.sh` appends two public
endpoints. The later prod-a-1500 ran with that. The TS bench also moved to SDK 4.0.1 (a public RPC provider's free plan refuses JSON-RPC batches of more than 3; fixed in 4.0.1) together with the RPC failover list.

### 9.8 Smaller findings
- smoke1 (bench rig): `SETTLE_TIMEOUT_MIN` 30 was below twice the 15-minute production batch timer; T4 gave up 1 min before
  the second seal and the close flushed it. For the production runs it needs 45 or more (or a shorter node timer).
- Bench chaos table keyed rows by name, so C7's two "b fee cap" rows collapsed in the dry run, and the c2 summary also
  collapsed them (it shows one "b fee cap" row); the run log has all three.
- Beacon redundancy: both configured beacons are the same provider behind two proxies.
- S-c: the ledger and the run log agree on 1536 votes in 6 transitions; the report's 3072 is the two-route accepted
  count, the same doubling as X1 (1280) and X4 (1200), not a disagreement.
- Production-readiness reviewer findings recorded in the live-bench ledger during the campaign: "a heartbeat close-out
  at grace end can run before a delayed peer landing reveals a grace extension" and "a Results event stops blob-replay
  retries while the actor is still behind". Both were addressed in hardening B (B1 close-out from chain truth, B4 Finalized keeps
  following the chain) before release v0.6.0; reviewer verdicts 12 and 14: PRODUCTION READY.

## 10. Spend against caps

µETH per scenario (summary tables).

c1 (caps from `budget.json`: L1 900 per key, OP 150 per key, organizer OP 300):

| scenario | seq-a L1 | seq-b L1 | seq-a OP | seq-b OP | organizer OP |
|---|---|---|---|---|---|
| w1/R | 56 | 56 | 1 | 1 | 0 |
| w1/S-16 | 100 | 100 | 1 | 1 | 0 |
| w1/S-2 | 42 | 42 | 1 | 1 | 0 |
| w1/S-c | 23 | 22 | 0 | 0 | 0 |

The c1 summary rows add up to seq-a 221 and seq-b 220 L1 µETH, 3 and 3 OP µETH, organizer 0 (S-8 is not in the table;
its last progress line shows 33 µETH per key; the ledger says about 245 per key for wave 1). Against the 900 cap
that is about a quarter.

c2 (final caps: L1 2000 per key, OP 144 per key, organizer OP 800; the first launch ran with L1 620 and organizer 250):

| scenario | seq-a L1 | seq-b L1 | seq-a OP | seq-b OP | organizer OP |
|---|---|---|---|---|---|
| w2/X1 | 275 | 275 | 5 | 4 | 0 |
| w2/X2-a | 132 | 132 | 2 | 0 | 0 |
| w2/X2-b | 94 | 94 | 0 | 1 | 0 |
| w2/X3 | 12 | 2 | 0 | 0 | 499 |
| w2/X4 | 16 | 16 | 0 | 0 | 0 |
| w2/X5 | 21 | 40 | 0 | 0 | 0 |
| w3/C1 | -2973 | 13 | 0 | 0 | 0 |
| w3/C2 | 11 | 11 | 0 | 0 | 0 |
| w3/C3 | 8 | 9 | 0 | 0 | 0 |
| w3/C4 | 27 | 37 | 0 | 0 | 0 |
| w3/C5 | 14 | 16 | 0 | 0 | 0 |
| w3/C6 | 7 | 7 | 0 | 0 | 0 |
| w3/C7 | 14 | 7 | 0 | 0 | 0 |
| w3/C8 | 16 | 10 | 0 | 0 | 0 |

Wave 2 summary rows add up to seq-a 550 and seq-b 559 L1 µETH, 7 and 5 OP µETH, organizer 499 (the owner transfer
that stopped X3). Wave 3 rows add up to seq-b 110 L1 µETH and, for seq-a, 97 without the C1 row plus 27 for C1 (seq-b 13 in C1), so wave
3 seq-a is 124 µETH. The c2 total is seq-a 550 + 124 = 674 and seq-b 559 + 110 = 669 L1 µETH.
The C1 cell for seq-a (-2973) is a bench netting artifact: `budget.json` inflows.l1[0] is 3400 µETH, which is +0.4 mETH
on 2026-10-15 at 18:2x (live-bench ledger) plus +3 mETH at 19:10 UTC (op-dkg ledger, recorded as a c2 inflow for the
production runway). C1 ran from 18:59:07 to about 19:30, so the 3 mETH landed inside C1 and its spend netted it:
-2973 + 3000 = 27 µETH actual seq-a L1 spend in C1.

Starting balances: c1 started with L1 1361.7 / 1000.0 µETH and OP 197.9 / 300.0 µETH on A / B (organizer OP 1303.8); c2
started with L1 1089.7 / 722.5, OP 192.9 / 294.9, organizer OP 9882.4. The spend of the TS production runs is not
tabulated in the TS reports.

## 11. TS SDK stage timings

All against registry 0x64d4...6916 on the production node (one node, prover z6) except smoke1 (bench rig, registry
0x7bfa...). In the production runs a vote settles only when the node's batch timer fires (production timer 15 min
for a partial batch), so submit-to-settled is bounded by the timer, not by proving.

| Run | Config | prove p50 / p95 | submit p50 / p95 | accepted to settled p50 / p95 / max | votes/min (first-seen) | result |
|---|---|---|---|---|---|---|
| smoke1 | 3 voters + 1 revote, nf 4, route both, 1 worker | 1.0 / 1.2 s | 0.0 / 0.0 s | 997.2 / 2175.2 / 2175.2 s | 0.2 | tally [1,0,1,1] equals expected; close-to-results 374.9 s |
| prod-a-500 | 500 voters + 10% revotes = 550 ballots, nf 8, 8 workers, route a | 3.4 / 3.8 s | 0.0 / 0.0 s | 252.4 / 943.0 / 1020.2 s | 35.7 | settled 550; bench result check failed (9.6); on-chain tally equals expected |
| prod-v4-smoke | - | - | - | - | - | died at start, RPC outage (9.7) |
| prod-v4-smoke2 | 3 voters, nf 2 | 1.0 / 1.2 s | 0.0 / 0.0 s | 947.0 / 948.0 / 948.0 s | 3.0 | settled 3; results wait timeout (9.7) |
| prod-a-1500 | 1500 voters, nf 16, 8 workers, 300/min, route a | 3.8 / 6.0 s | 0.0 / 0.0 s | 387.7 / 990.9 / 1058.9 s | 61.4 | settled 1500, results = tally |

Stage walls:

| Stage | prod-a-500 | prod-a-1500 | smoke1 |
|---|---|---|---|
| create | 1.7 s | 5.9 s | 2.0 s |
| set grace | 5.6 s | 6.0 s | 2.1 s |
| prove wall (= submit wall) | 235.4 s | 785.0 s | 5.3 s |
| follow (settle wait) | 1255.1 s | 1826.2 s | 1805.6 s |
| close tx | 1.8 s | 6.4 s | 6.3 s |
| close to results | not reached (9.6) | 353.2 s | 374.9 s |

Further numbers. prod-a-500: accepted to aggregated p50/p95 143.4 / 853.0 s, accepted to processed (100 samples) 882.9
/ 896.6 s, batches of 450 (at the batch maximum), 96 and 4 votes (ledger). prod-a-1500: accepted to aggregated 227.3 /
880.9 s, accepted to processed (813 samples) 344.6 / 463.0 s; 190 sweeps, 0 poll errors. Audit in all runs: errored,
unexpected, lost, stale, refused, uncertain, never admitted, failed proofs, skipped revotes all 0, poll 404 0.
prod-a-1500 results: 16-field tally [99,94,94,80,94,100,103,89,85,92,123,85,100,81,88,93] equal to expected, 1500
voters. smoke1: two sequencers raced on the bench rig (B won both batches, A synced in 3 s); preflight items (pins,
grace params, voters order, poll 404, results wait) passed.

## 12. Cleanup

Task 24 step 1 done: `cancel --all` reported "canceled 0 of 15 processes"; every c2 process was in Results. The
unit count of step 1 was not given to me and is not stated.

## 13. Open issues

1. X3 (5 skewed race rounds) has no live result: it ran 1 round before the cap stop and was not rerun. Covered only
   by the anvil race dry (2 of 2).
2. Ceiling data is wave 1 only and covers nf 2 and 16. nf 8 (S-8) has no ceiling cell because the OOM interrupted it;
   its votes were recovered, but no steady rate was measured on the fixed node. No cell was bound by the 240 s floor.
3. Beacon redundancy was not tested for provider failure (two proxies of one provider); the B Ethereum read RPC had no
   second entry to fail over to.
4. The bench nets a mid-scenario inflow into that scenario's spend (bench bug): C1's seq-a.l1 cell reads -2973.
5. The TS runs measure the production node with a 15-minute batch timer, so their settle times say nothing about the
   proving ceiling; the 1500-vote run settled at 61.4 votes/min. The bench's own result check (SDK 3.x base) is wrong
   against the new registry; only the SDK 4.0.1 rebase fixes it. smoke1 and prod-a-500 keep the 3.x behavior.
6. Source gaps: binary sha256s and image digests of the setup are not in the files I read; no per-batch breakdown for prod-a-1500 in the report; the prod-v4-smoke run has only a log; spend of the
   TS production runs is not tabulated; S-8's final spend is not in the summary.
