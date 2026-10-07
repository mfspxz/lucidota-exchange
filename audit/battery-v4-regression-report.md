# REDTEAM-2 — Battery v4 Regression Report

**Group:** battery-v4-regression (replan / quota / integrity / concurrency)
**Window:** 2026-10-07 03:50–04:18 UTC
**Method:** live reproduction vs the live governor; real User-Agent
(`lucidota-redteam2-regression/1.0`); 429-compliant (zero 429s hit).
**TERMINATED EARLY** by Zackary's D1 EMERGENCY FREEZE (2026-10-07 ~04:18Z):
zero live-fire vs governor/D1 until 2026-10-08 00:00 UTC. All background
probes killed; this report uses already-collected data only.

## Version drift (material)

The governor was redeployed **three times mid-window** by other lanes:
`0.6.6 → 0.6.7 → 0.6.8 → 0.6.9`. Version pinned per test below.
Notable: v0.6.8 shipped the **R1 double-accounting fix**
(atomic active→completing flip before all side effects; sweeper resets stuck
`completing`; replan treats `completing` as live), and v0.6.7 was the voting
lane's merge. Local `worker.js` is stale at 0.6.6 — all code claims below are
verified against live behavior, not the local file.

## (b) QUOTA enforcement — PROVEN (6/6), ran on v0.6.6→v0.6.7

| case | max_cost | spend_authority | result |
|---|---|---|---|
| mc1-saNone | 1 | — | **blocked** |
| mc0-saNone | 0 | — | deferred (normal) |
| mc1-sa05 | 1 | 0.5 | **blocked** |
| mc04-sa05 | 0.4 | 0.5 | deferred (normal) |
| mc0-sa0 | 0 | 0 | deferred (normal) |
| mc1-sa1 | 1 | 1 | deferred (normal) |

Both blocked jobs carry the exact refusal reason in their receipt decision:
- `job_b4reg-quota-mc1-saNone-1791345097` → `rcpt_muxkmghji7zvds`:
  `"resource":"nim-nvidia","gate":"COST","reason":"declared max_cost $1 exceeds spend_authority $0","taxonomy":"POLICY_FAILURE","retryable":false`
- `job_b4reg-quota-mc1-sa05-1791345110` → `rcpt_muxkmp59hhi19g`:
  `... "reason":"declared max_cost $1 exceeds spend_authority $0.5" ...`

**Observability note:** the exact reason is only in the receipt `decision`;
the submit-response `governor_trace` shows only `"1 candidate(s) rejected at
COST"`. Enforcement is correct; the trace is lossy.

**Verdict: PROVEN.** Boundary holds on both sides (block when
`max_cost > spend_authority`, pass when `<=`).

## (c) INTEGRITY stability — PASS, 18 samples / ~18 min (03:51–04:09Z)

`/v1/health` + `nim-nvidia` evidence sampled every 60s
(`/tmp/integrity-watch.jsonl`):
- `evidence_state`: **VERIFIED for all 18 samples — never moved.**
- `state`: oscillated AVAILABLE ↔ DEGRADED; every demote-log trigger was
  `health-transition` (health observations), never an evidence demotion.
- The state flapping correlates with the live redeploy windows
  (0.6.6→0.6.9), not with the v0.6.3 failure mode.

**Verdict: PASS.** No non-stationary INTEGRITY boundary movement like the
v0.6.3 claimed-probe poisoning (which demoted `evidence_state` via false
Worker probe failures). The v0.6.3 fix (skip Worker revalidation probe for
claimed adapters) holds through v0.6.9.

## (d) CONCURRENCY — PASS on v0.6.9

### R1 double-complete — PASS (v0.6.8 fix confirmed live)
- `job_b4reg-r1-1791345773`, `claim_muxl189ldtnzrf` (swarm-claimant-01).
- Barrier-synced double complete: **one 200 winner, one 409**
  `"claim not active (status=completing) — completion rejected"`.
  The concurrent loser is rejected by the atomic active→completing flip —
  it does not double-complete.
- Sequential replay of the same claim: **200, `idempotent:true`,
  `"already completed — no duplicate accounting"`**, same receipt
  `rcpt_muxl1gigcw3jrk`.
- Accounting rows: **1** after race AND replay. Receipts: 2
  (delegation `rcpt_muxl11p0bm9a6j` + completion). No double-accounting.

**Verdict: PASS.** The v4-found HIGH (R1 double-accounting) is fixed and
regression-verified on v0.6.9.

### R2 claim race — PASS
- 4/4 barrier-synced 2-worker races (swarm-claimant-01 vs nim-claimant-02)
  on `res_localswarm`, each a true race (single unclaimed delegation).
- Winners alternated (swarm, swarm, nim, nim). **Zero same-job
  double-claims.** The conditional-UPDATE atomic win holds.
- Own-job wins were completed `FAILED_RETRYABLE` immediately (clean;
  rejoin the replan queue).

**Verdict: PASS.**

Race-hygiene note: one earlier non-race attempt (crashed script) left two
abandoned claims — `job_ir1791345524-a3` (another lane's "ir" cognitive
probe, claim by swarm-claimant-01) and `job_b4reg-conc-r2-1791345568`
(nim-claimant-02). Both self-heal via the zombie sweep → TIMEOUT → REPLAN
path; no fabricated outcomes were written.

## (a) REPLAN path — PARTIAL (freeze cut the watch)

**Forced FAILED_RETRYABLE on a real claimed job: PROVEN.**
- `job_b4reg-replan-1-1791345400` (cognitive → `res_localswarm`),
  claimed as swarm-claimant-01 → `claim_muxktbjsxrrbng`,
  completed `FAILED_RETRYABLE` → receipt `rcpt_muxkuvwdf1pynb`;
  job → `deferred`, `defer_count` 0→1, lease `lease_muxksykf82e1re` released.
- (res_localswarm chosen deliberately: no fleet claimant races claims there,
  unlike nim-nvidia where the 120s hook snatches jobs in ~1–2 min.)

**Cron replan of that job: UNKNOWN.** At 04:12Z it sat at position **52/74**
in the effective replan queue (D1 query; cron takes 2 oldest per 5-min
tick) — ETA ~2h. The :15-tick watch (front jobs
`job_probe-20261007-030118-F4-30`…`-44`, all deferred/defer 1) was killed
by the freeze before the AFTER snapshot.

**Replan-failure receipts: 0 in DB** (queried `decision LIKE
'%"replan":"failed"%'`). The v0.6.1+ catch→receipt code path is live in
code but has never fired — consistent with the v4 replan group's finding.
**UNPROVEN live** (not disproven; no throw has occurred to exercise it).

Context from the v4 replan group (already-collected, their report):
replan latency ≈ 1 cron tick for front-of-queue jobs (4m17s, 4m51s
measured); the "v0.6.1 silent failure" premise was DISPROVEN for
`job_muxji956wbg7su`.

## Test count

6 quota + 18 integrity samples + 2 R1 (race + replay) + 4 R2 races +
1 forced-retryable + 1 DB replan-failure scan = **32 test events**.
Zero 429s encountered.

## Open / recommended follow-ups (post-freeze)

1. Re-verify the cron replan of `job_b4reg-replan-1-1791345400`
   (defer_count 1→2, new lease, new receipt) after 2026-10-08 00:00 UTC —
   or watch front-of-queue jobs for one tick.
2. The replan-failure receipt path still needs a synthetic throw to prove
   end-to-end (suggest unit-level, not against the live fleet).
3. `governor_trace` drops the exact per-candidate COST/QUOTA reason
   (only in receipt decision) — observability gap, not a security gap.
4. Confirm the two abandoned claims (`job_ir1791345524-a3`,
   `job_b4reg-conc-r2-1791345568`) replanned cleanly post-freeze.

## Artifacts (local, no network touched after freeze)

- `/tmp/integrity-watch.jsonl` — 18 samples
- `/tmp/quota-results.json` — 6 cases
- `/tmp/r1-results.json`, `/tmp/r2-results.json`
- `/tmp/replan-watch.py` (BEFORE snapshot only; killed pre-tick)
- `/tmp/quota-test.py`, `/tmp/r1-test.py`, `/tmp/r2-test.py`,
  `/tmp/integrity-watch.py` — battery scripts, reusable post-freeze
