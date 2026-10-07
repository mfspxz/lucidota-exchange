# Adversarial Battery v3 — Exchange Governor v0.6.1

**Target:** live governor `https://lucidota-exchange.cutecity.workers.dev` (v0.6.1)
**Date:** 2026-10-07
**Scope:** NEW v0.6.1 defenses only — backpressure (429 queue-full at depth>=40/resource),
`/v1/claim` expiry filter, replan receipts.
**Constraints:** $0 spend, no destructive actions, bounded submissions, tiny inputs.
**Method:** live reproduction against the production governor + code audit of the
deployed `worker.js` (v0.6.1, `~/workspace/exchange/worker.js`).

## Verdicts

| Test | Attack | Verdict | Severity |
|------|--------|---------|----------|
| T1 | Verify-endpoint backpressure bypass | **PROVEN** | HIGH |
| T2 | Parallel-submission TOCTOU on threshold | **PROVEN** (overshoot=1) | LOW |
| T3 | Idempotency-key games | **PROVEN** (behaves correctly — not a bypass) | INFO |
| T4 | Resource hopping / capability smuggling | **DISPROVEN** (practical) / note (architectural) | INFO |
| T5 | Claim expiry filter + claim→complete race | Filter PROVEN / race **UNKNOWN** | LOW |
| T6 | 429-ignoring producer resilience | **PROVEN** (resilient) | INFO |
| T7 | Replan receipt observability | **UNKNOWN** (no live replan in window) | INFO |

---

## T1: Verify-endpoint backpressure bypass — PROVEN (HIGH)

**Claim:** `POST /v1/resources/:id/verify` mints leases without any backpressure
check, bypassing the 429 queue-full defense.

**Reproduction:**
1. Filled the nim-nvidia queue via parallel `/v1/job` submits until 429:
   `{"error":"queue-full","resource":"nim-nvidia","queue_depth":40,"threshold":40,...}`
2. While `/v1/job` returned 429, called:
   `POST /v1/resources/nim-nvidia/verify`
3. Response minted `lease_id: lease_muxjipfwdqnaqy` for `job_muxjip80wjxmj7`
   — a lease was created despite the queue being full.

**Root cause (code):** the verify endpoint (`worker.js:1877`) calls
`delegateExecution()` directly. The backpressure check lives in `runJob()`
(`worker.js:1477`), which the verify path never calls. Three lease-minting
paths exist; two go through `runJob()` (protected), one does not.

**Impact:** any holder of the Exchange key can grow the unclaimed queue
without bound via the verify endpoint, defeating the v0.6.1 backpressure.
Mitigating factor: the verify path requires the Exchange master key (not a
worker credential), so the threat is a misbehaving trusted producer, not a
remote worker. The probe harness (which caused the original incident) uses
the Exchange key.

**Fix:** gate the verify endpoint's `delegateExecution` on
`unclaimedQueueDepth()` — return 429 when full, same as `/v1/job`.
One-line conceptual change; no protocol impact.

## T2: Parallel-submission TOCTOU on the threshold — PROVEN (LOW)

**Claim:** the backpressure check is check-then-act (SELECT depth, then
INSERT+lease in separate statements). Concurrent submits can overshoot 40.

**Reproduction:** 64 parallel `/v1/job` submits (8 threads): 24 accepted,
40 rejected. Rejections reported `queue_depth: 41` — depth exceeded the
threshold by 1. The overshoot equals the number of requests that passed the
check before any of them minted a lease.

**Impact:** LOW. Overshoot is bounded by in-flight concurrency (observed: 1
under 8-way parallelism; D1's single-writer serializes aggressively). The
threshold is a soft cap, not a hard cap — acceptable for backpressure, but
it must not be relied upon as an exact invariant.

**Fix (optional):** make the check atomic via a conditional INSERT
(`INSERT ... SELECT ... WHERE (SELECT COUNT(*)) < 40`), or accept the soft
cap and document it.

## T3: Idempotency-key games — PROVEN correct (INFO, not a bypass)

**Reproduction:**
- Double-submit with key `b3-t3-dedup`: second returns
  `{job_id: "job_b3-t3-dedup", deduplicated: true}` — no new job, no queue growth.
- Submit same key with a *conflicting* spec (different organelle, evil input):
  returns the ORIGINAL job unchanged. Standard idempotency semantics; the
  conflicting spec is ignored, not merged.
- 429'd submissions DELETE the job row, so a retry with the same key after
  a 429 is treated as new (correct — no phantom dedup blocking legitimate retry).

**Verdict:** the idempotency layer does not provide a queue-full evasion path.
Recorded as PROVEN-correct.

## T4: Resource hopping / capability smuggling — DISPROVEN practical (INFO)

**Claim:** steer `govern()` across N resources to get N×40 queue depth.

**Reproduction:** probed capability→resource selection:
`inference` → `nim-nvidia`; `http-check` → `cutecity-health`.
The backpressure threshold is per-resource, so *in theory* an attacker could
fill each claimed resource to 40.

**Why it fails in practice:** `nim-nvidia` is the only usable *claimed*
(delegated) resource. `cutecity-health` / `httpbin-json-fingerprint` /
`exchange-self` are Worker-executed (inline) — their jobs execute immediately,
never queue, so the backpressure check is vacuous for them. There is no
second delegated resource to hop to.

**Architectural note:** if a second delegated resource is ever registered,
the total queue bound becomes 40×N. Consider a global cap or per-resource
thresholds scaled by drain capacity at registration time.

## T5: Claim expiry filter — PROVEN; claim→complete race — UNKNOWN (LOW)

**Filter (PROVEN by code + live behavior):** `/v1/claim` now includes
`AND l.expires_at > ?` (worker.js, claim handler). No expired-lease claims
were observed during the battery; the claimant's VALIDATE step (which
refuses expired delegations) never fired on an expired lease.

**Race (UNKNOWN):** the atomic win is
`UPDATE leases SET state='claimed' WHERE id=? AND state='active'` — it does
**not** re-check `expires_at`. A lease expiring between the SELECT and the
UPDATE (microsecond window) can still be claimed. The claimant would refuse
it at VALIDATE and `/v1/complete` would 409 (W4 lease-expiry check), so the
blast radius is one wasted claim cycle, not double-execution or forgery.
Not reliably reproducible live; defense-in-depth fix:
`... AND state='active' AND expires_at > ?` on the UPDATE.

## T6: 429-ignoring producer resilience — PROVEN (INFO)

**Reproduction:** 40 rapid-fire `/v1/job` submits against a full queue —
all returned 429, `queue_depth` stable at 40–41, no job rows accumulated
(429 path DELETEs the row), no quota burn (`govern()` is read-only;
`quotaConsume` only fires on actual execution), no state corruption.

**Verdict:** the governor degrades gracefully under a misbehaving producer.
The 429 path is side-effect-free except for the (correct) job-row DELETE.

**Addendum — backpressure is not sticky:** a 2-minute depth monitor showed
`queue-full` at depth 42 → 40, then clean accepts (`deferred`) once the
claimants drained below 40. The gate opens and closes correctly around the
threshold; no manual reset needed.

## T7: Replan receipt observability — UNKNOWN (INFO)

Scanned the rolling 50-receipt window: zero replan-related receipts. The
replan-failure receipt code path exists in the cron handler (worker.js),
but no cron replan failure occurred during the battery window, so live
proof is pending. The replan *success* path already wrote receipts via
`delegateExecution` (pre-existing).

---

## Fixes recommended (priority order)

1. **T1 (HIGH):** gate `POST /v1/resources/:id/verify` on `unclaimedQueueDepth()`
   before `delegateExecution()`. This is the only unprotected lease-minting path.
2. **T5 (LOW):** add `AND expires_at > ?` to the claim-winning UPDATE.
3. **T2 (LOW):** document the threshold as a soft cap, or make it atomic.
4. **T4 (note):** when registering a second delegated resource, add a global
   queue cap or scale per-resource thresholds.

## Artifacts

- Battery script: `~/workspace/exchange/worker-battery-v3.py`
- Raw results: `/tmp/battery-v3-results.json`
- Live reproductions: T1 (lease `lease_muxjipfwdqnaqy`), T2 (depth 41),
  T3 (job `job_b3-t3-dedup`), T6 (40× 429, depth stable)
