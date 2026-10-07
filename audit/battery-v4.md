# Adversarial Battery v4 — Exchange Governor v0.6.2 → v0.6.3

**Target:** live governor `https://lucidota-exchange.cutecity.workers.dev`
**Date:** 2026-10-07
**Scope:** replan path (T7 UNKNOWN from v3), INTEGRITY non-stationarity,
QUOTA gap fix, deeper concurrency races.
**Method:** live reproduction + code audit, 3 parallel groups per the
parallelization directive (replan / integrity / concurrency).
**Constraints:** $0 spend, no destructive actions, 429-compliant probing.
**D1 FREEZE:** all live probing halted 2026-10-07 ~04:15 UTC — Cloudflare at
87% of 5M daily rows_read, resets 2026-10-08 00:00 UTC. Report written from
data collected before the freeze.

## V4-A: INTEGRITY non-stationarity — ROOT CAUSE FOUND + FIXED (v0.6.3)

**Claim (from probe harness):** the INTEGRITY gate boundary "moved" between
02:35–03:05Z — input (9925 chars, max_tokens=1500) failed ×3, then passed.

**Root cause (code, worker.js INTEGRITY gate):** the gate revalidates stale
resources via `adapter.probe(r)`. The `claimed` adapter's `probe()` **always**
returns `{ok:false}` (no credential held by the Worker). So whenever
nim-nvidia's last health observation aged past `staleAfterEv` (15 min for
VERIFIED/AVAILABLE):

1. Job submitted → INTEGRITY sees stale → runs probe → probe fails (guaranteed)
2. `observe(db, r.id, ok=false, ...)` writes a FAILED observation
3. Failed observation demotes evidence_state → subsequent jobs fail INTEGRITY
   ("evidence X changed since shortlist") — the "boundary"
4. A claimant later executes successfully → observation ok=true →
   autoPromote → evidence_state recovers → jobs pass — the boundary "moves"

The input ratio was a red herring. The state variable is
**health_observations freshness + evidence_state**, and the trigger is the
**guaranteed-failing revalidation probe**.

**Fix (v0.6.3, live):** the INTEGRITY gate skips the Worker revalidation
probe for claimed adapters (their freshness is judged by claimant
observations; staleness demotion is the cron's `demoteCheck`). No false
failure is written; the resource is never poisoned.

## V4-B: QUOTA gap — FIXED (v0.6.3)

**Claim (from probe harness):** declared `max_cost` not validated against
spend authority — `max_cost=1` passed QUOTA/COST with full delegation.

**Fix (v0.6.3, live):** COST gate now rejects when
`(spec.max_cost ?? 0) > (spec.spend_authority ?? 0)` — POLICY_FAILURE,
non-retryable. Verified live:
- `max_cost=1`, no spend_authority → `blocked`,
  reason: "declared max_cost $1 exceeds spend_authority $0"
- `max_cost=0` control → `deferred` (delegated normally)

## V4-C: Replan path — LIVE EVIDENCE (group running)

`job_muxji956wbg7su`: claimed → FAILED_RETRYABLE (res_localswarm
ternary_verdict -1) → lease released → job deferred → **cron replan fired**
(defer_count 0→1). The replan ran under v0.6.1 (silent failure — no new
lease, no receipt), confirming T7's "UNKNOWN" was a real silent-failure
path. v0.6.2 added replan-failure receipts; the replan group is watching
for the next cron cycle to capture the exact runJob error.

## V4-D: Concurrency races — initial probe (group running deeper)

- R2 claim race: 2 workers both `claimed=True` — **test flaw**: job_ids not
  compared (queue may have held 2+ jobs). Re-running with job_id verification.
- R1 double-complete: both 200, second `idempotent=false` — investigating
  whether double accounting occurred or the flag is unset.
- R3 backpressure under 8-way parallel: 32 accepted, 0 rejected (queue was
  drained) — gate not stressed; concurrency group holding depth 35–38.

## Fixes shipped (v0.6.3)

1. INTEGRITY: skip Worker revalidation probe for claimed adapters.
2. COST: `max_cost <= spend_authority` enforced.

## V4-E: R1 double-complete — HIGH BUG FOUND + FIXED (v0.6.8)

**Claim (concurrency group):** barrier-synchronized double `/v1/complete`
on one claim → both 200, different receipt_ids, 2 receipts + 2 accounting
rows. True double execution, not a flag artifact.

**Root cause (code):** `completeClaimed` idempotency was check-then-act
(SELECT status, ~40 statements, then UPDATE). Two concurrent requests both
saw `active`, both completed. Breaks Worker Protocol v1 invariant W2.

**Fix (v0.6.8, live):** atomic conditional flip `active→completing` FIRST,
before ALL side effects — one UPDATE decides the winner, mirroring the
claim path. Loser replays the winner's result. Final UPDATE completes
from `completing` only. Sweeper resets stuck `completing` (>15min) to
`active`. Replan live-check treats `completing` as live work.

**Verified live:** double-complete race → r1 200 (winner), r2 200
`idempotent=true` with the SAME receipt_id; 1 completion receipt,
1 accounting row. No double accounting.

**Collision note:** the voting lane deployed v0.6.7 to the same worker
mid-fix; my v0.6.3 INTEGRITY/QUOTA fixes survived (built on top), the
R1 fix was re-applied to v0.6.7 → shipped as v0.6.8.

## Group reports

- `battery-v4-concurrency.md` — DONE: R2 PASS (atomic claim holds, 12 rounds,
  0 double-grants); R1 HIGH found + fixed (v0.6.8); R3 partial (shared queue,
  needs cross-lane coordination).
- `battery-v4-integrity.md` — DONE: PASS. 10/10 jobs over 20 min, zero
  INTEGRITY rejections; v0.6.3 fix verified live and stable.
- `battery-v4-replan.md` — INCOMPLETE (D1 freeze): replan group stopped
  2026-10-07 ~04:15 UTC per D1 emergency (87% of 5M rows_read). What was
  established before the freeze: job_muxji956wbg7su showed defer_count 0→1,
  proving the cron replan FIRES; the replan ran under v0.6.1 (silent failure
  — no new lease, no receipt), confirming T7's UNKNOWN was a real
  silent-failure path. The v0.6.2 replan-failure receipt was never captured
  live. The runJob-throws-during-replan question remains OPEN — first
  priority when D1 resets 2026-10-08 00:00 UTC.
