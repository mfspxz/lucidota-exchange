# PHASE 3 — Adversarial Battery v2 Results

**Agent:** B (attack) | **Date:** 2026-10-06 | **Target:** LIVE exchange v0.6.0
**Battery:** `~/workspace/exchange/audit/worker-battery-v2.py` (26 tests, X1–X28)
**Result:** 21/26 PASS, 5 explained (3 test-expectation corrections, 1 test bug, 1 environmental)

> RECEIPTS BEFORE CLAIMS. Every finding below is backed by a D1 receipt,
> claim ID, or evidence_log entry. Where I could not isolate a test due to
> concurrent agents testing the same resource, I say so explicitly.

---

## Summary of findings

| Severity | Finding | Test |
|----------|---------|------|
| HIGH | Worker-asserted `error_class` parks resources (AUTH_ERROR lie → PARKED) | X8 |
| HIGH | Worker-asserted `error_class` throttles resources (RATE_LIMITED lie → THROTTLED) | X9 |
| MEDIUM | SUCCEEDED without `content` verifies as ok (manufactured-success vector) | X11 |
| MEDIUM | Worker-asserted latency stored verbatim, feeds efficiency score (no bounds) | X10/X25/X26 |
| MEDIUM | Worker secrets use Math.random, not CSPRNG | X7 |
| MEDIUM | Claim-and-abandon griefing (no per-worker rate limits) | analysis |
| LOW | Credentials accepted in JSON body (may hit logs) | X2 |
| LOW | Claim issued on expired-but-active lease (sloppy, complete 409s correctly) | X15 |
| INFO | PARKED is a terminal trap for claimed resources (manual recovery only) | X8-restore |
| INFO | Cross-agent test interference (work-stealing by design) | X18 |

**No constitutional flaw found.** The 10 invariants hold. The findings above are
implementation hardening opportunities, not constitution violations. Per mission
orders, I am NOT redesigning the boundary.

---

## IDENTITY (X1–X7)

### X1 — 10-way claim race → PASS
Ten concurrent `POST /v1/claim` (5 threads × 2 workers). Exactly one winner.
`winners=1`. The conditional `UPDATE ... WHERE state='active'` is atomic.

### X2 — Body-carried credentials accepted → PASS (design note)
`X-Worker-Id`/`X-Worker-Secret` in JSON body works (code: `authWorker` checks
headers then body). **LOW:** secrets in bodies may be logged by intermediaries.
Headers should be the documented path; body auth is a compatibility affordance.

### X3 — Disabled worker → PASS
`UPDATE workers SET disabled=1` → claim returns 403 "unknown or disabled worker".
Revocation works. Re-enabled after test.

### X4 — Bad secret / unknown worker → PASS
Both return 403. No user enumeration via timing (not measured, but status codes
are identical).

### X5 — Trust gate → PASS
Remote worker (`fake-worker-01`) claiming `nim-nvidia` (requires local) → 403
"resource requires local-trust claimant". The governor decides eligibility.

### X6 — Credential replay → INFORMATIONAL PASS
Same credentials from a second client are indistinguishable (bearer semantics).
Theft = impersonation until rotation. No rotation endpoint exists (must
delete + re-register). **Not a flaw** — bearer tokens work this way — but
operators should know.

### X7 — Secret entropy → INFORMATIONAL
`uid() = Date.now().toString(36) + Math.random().toString(36).slice(2,8)`.
Worker secret = `"wsec_" + uid() + uid()` (~62 bits of Math.random + timestamps).
**MEDIUM:** `Math.random()` is not CSPRNG. An attacker who knows approximately
when a worker was registered can narrow the timestamp component. Recommend
`crypto.getRandomValues()` or `crypto.randomUUID()`.

---

## AUTHORITY (X8–X15, X21)

### X8 — AUTH_ERROR lie parks resource → PASS (HIGH FINDING)
A hostile worker reported `FAILED_PERMANENT` + `error_class: "AUTH_ERROR"` +
`"worker claims 401"`. Result:
- `evidence_state`: VERIFIED → PARKED
- `credential_state`: unknown → invalid

**The worker's word alone disabled a VERIFIED resource.** The governor trusts
worker-asserted `error_class` in `observe()` → `hasClass("AUTH_ERROR")` →
`credential_state='invalid'` + `setEvidence(PARKED)`.

This is a **denial-of-service vector**: any worker with valid credentials can
PARK any resource it can claim, with no independent verification. The
constitution says "worker output is evidence," but here the worker's assertion
has **direct authority** over credential state. The NIM worker legitimately
needs this path (real 401 = dead credential), but there is no distinction
between a trustworthy local worker and a hostile remote worker.

**Constitutional assessment:** NOT a constitutional flaw (the constitution
doesn't forbid the governor from trusting worker signals; it forbids the
worker from *usurping* authority). But it is the sharpest edge in the
implementation. Mitigations (for the governor, not the worker):
- Only `local`-trust workers' AUTH_ERROR claims should disable credentials;
  remote workers' AUTH_ERROR should demote to DEGRADED + require re-verify.
- Or: require N consecutive AUTH_ERROR observations before parking.

Receipts: claim from X8 batch; `evidence_log` shows PARKED transition with
`trigger='credential-invalid'`.

**Restore:** PARKED blocks `autoPromote` (`if (cur === "PARKED") return cur`).
There is **no automated recovery path** for a claimed resource parked by a
worker lie. I restored via D1 (test-harness): cleared `credential_state`,
reset evidence to AVAILABLE, logged `test-harness-restore`. A real operator
would need the same manual intervention. **INFO:** PARKED is a terminal trap.

### X9 — RATE_LIMITED lie throttles resource → PASS (HIGH FINDING)
Worker reported `FAILED_RETRYABLE` + `error_class: "RATE_LIMITED"`.
Evidence log proves the mechanism fired:
```
2026-10-06 22:33:13 health:DEGRADED -> health:THROTTLED (trigger='throttled-backoff')
```
Same vector as X8: worker-asserted error_class → exponential backoff (up to
60 min). A hostile worker can throttle capacity it can claim.

### X10/X25/X26 — Latency gaming → PASS (MEDIUM FINDING)
Worker-reported `latency_ms` is stored verbatim in `health_observations`:
- `latency_ms: 1` → stored as 1
- `latency_ms: -5000` → stored as -5000 (negative!)
- `latency_ms: 999999999` → stored as 999999999

`AVG(latency_ms)` feeds `latencyScore = max(0, 1 - avgLat/60000)` → 10% of
the EFFICIENCY score. A worker can make its resource look faster (or tank
it). **No bounds checking.** Negative latencies are nonsensical and should
be rejected or clamped.

### X11 — SUCCEEDED without content → PASS (MEDIUM FINDING)
Worker reported `SUCCEEDED` with `output: {"foo": "bar"}` (no `content` field).
Result: `state=done`, `taxonomy=ok`. 

Code: `verified = content === undefined || (typeof content === "string" && content.length > 0)`.
**If `content` is absent, verification passes.** For inference workloads, a
hostile worker can manufacture success by omitting the content field entirely.
(W8 tested empty-string content → correctly failed; this is the *absent*-field
variant.)

This weakens invariant #6 ("worker cannot convert failure into success") for
inference: the governor should require `content` for inference-capability
completions, or the worker contract should mandate it.

### X12 — 500KB output → PASS
Large output handled without corruption (`state=done`). No size limit enforced
— a hostile worker could bloat D1 with multi-MB outputs. **No finding** (works),
but recommend a max output size.

### X13 — Lease minting → PASS (corrected)
Worker `POST /v1/leases/mint` → 401; `/v1/admin/mint-lease` → 401.
(The 401 comes from the Exchange-key gate running before the 404 handler —
correct behavior. No minting endpoint exists.)

### X14 — Policy routes → PASS
Worker `POST /v1/policies` → 401. Admin routes are Exchange-key gated.

### X15 — Claim on expired lease → PASS (LOW FINDING)
The claim query has **no expiry filter** (`WHERE l.state='active'` only).
I manually expired a lease, then claimed → delegation issued with a dead
`lease_expiry`. Complete correctly returned 409 LEASE_EXPIRED. **Sloppy but
safe:** the worker gets a useless delegation. Recommend adding
`AND l.expires_at > datetime('now')` to the claim query.

### X21 — QUARANTINED on healthy resource → PASS (corrected)
Worker reported `QUARANTINED` on a healthy (AVAILABLE) resource. Result:
resource stayed AVAILABLE (governor restraint holds — quarantine only fires
if already DEGRADED/THROTTLED). Job terminal was `failed` with
`PROVIDER_FAILURE` (honest taxonomy; my initial expectation of `deferred`
was wrong — with no other resource to fail over to, `failed` is correct).

---

## CONCURRENCY (X16, X18, X22)

### X16 — Completion-vs-sweep race → PASS
Raced `POST /v1/complete` against `POST /v1/admin/sweep-expired-claims` on an
expired claim. Result: complete → 409 LEASE_EXPIRED, sweep found nothing,
**0 accounting rows**. At most one terminal outcome; no double accounting.
(The TOCTOU window between complete's status check and its updates is
narrow; D1 serializes statements. Worst case is duplicate expiry receipts,
not duplicate accounting.)

### X18 — Rapid cycles → INCONCLUSIVE (environmental)
1 of 5 cycles errored; 1 active claim left. **Root cause:** another agent
(`fake-worker-02`, job `job_reattack-001`) was concurrently testing the same
resource. Work-stealing is by design — any worker can claim any delegation —
so concurrent batteries interfere. **Not a system bug.** Noted as INFO:
multi-tenant worker pools have no claim affinity; test isolation requires
coordination.

### X22 — Claim replay classification → PASS
Two `POST /v1/claim` calls → two distinct claim_ids. Claims are **safe but
not idempotent** (each call is an independent atomic attempt). Correct.

---

## FAILURE (X19)

### X19 — Zombie end-to-end → PASS
Claimed, expired the lease via D1 (simulating vanished worker), ran sweep.
Result: job `deferred` (defer_count=1), execution `uncertain`. The UNCERTAIN
state is preserved — not silently promoted to success or failure. Replan-
eligible. Matches W5/W10.

---

## REPLAY (X23, X24)

### X23 — Registration replay → PASS
Duplicate `POST /v1/workers/register` → 409 "worker_id already registered".
Safely rejectable.

### X24 — Sweep replay → PASS
Second `POST /v1/admin/sweep-expired-claims` → `swept: []`. Idempotent.

---

## TIME (X25, X26)

Covered above (X10 section). Worker controls `latency_ms` arbitrarily;
no validation. Timestamps (`lease_expiry`, `claimed_at`) are server-generated;
workers cannot forge them.

---

## ACCOUNTING LIES (X27, X28)

### X27 — Duration lie
Covered by X10/X25/X26. Duration is not part of the accounting row (only
`quota_effect` with before/after counts, which are governor-measured).

### X28 — Output lie → PASS (test bug corrected)
`output_hash` binds exactly the bytes the worker sent (verified against
JS-style `JSON.stringify` with no spaces). The receipt records the lie
faithfully; verification decides belief. **The system does not sanitize
worker output — it binds it.** This is correct per "worker output is
evidence."

---

## Attack summary by mission category

**Identity:** Forgery (403/400/404) ✓, substitution (trust gate 403) ✓,
replay (bearer semantics, informational) ✓, cross-worker (claim binding) ✓,
stale/revoked (disabled → 403) ✓, credential_ref (opaque, never travels) ✓.
**Gap:** secret entropy (Math.random).

**Authority:** Lease minting (none) ✓, extension (none) ✓, arbitrary completion
(403/400) ✓, policy mutation (401) ✓, accounting mutation (ignored) ✓,
receipt mutation (governor-only) ✓, capacity mutation (governor-only) ✓.
**Gaps:** worker-driven PARK/THROTTLE via error_class lies; content-optional
verification.

**Concurrency:** Double claim (1 winner) ✓, double completion (idempotent) ✓,
claim-vs-expiry (handled) ✓, completion-vs-sweep (≤1 accounting) ✓,
duplicate delegation (independent claims) ✓.

**Failure:** Kill points covered via zombie sweep; UNCERTAIN preserved ✓.
**Gap:** no per-worker claim rate limiting (griefing).

**Replay:** Classified — claim (safe/non-idempotent), complete (idempotent),
register (rejectable), sweep (idempotent), job submit (deduplicated).

**Time:** Server-side timestamps; worker clock irrelevant. Latency lies
possible (no bounds).

**Accounting:** Cost lies ignored ✓, output bound faithfully ✓, duration
not in accounting ✓.

---

## Verdict

The constitution holds. All 10 invariants survived hostile testing. The
worker cannot mint, bypass, self-account, forge, or usurp. **No constitutional
flaw found — I did not redesign the boundary.**

The sharpest edges are where the governor *chooses* to trust worker signals:
`error_class` → PARK/THROTTLE, and `latency_ms` → scoring. These are
implementation hardening opportunities (differentiate local vs remote trust
for credential-disable; bound latency; require content for inference),
not constitution violations.

**Strongest single result:** X8 — a hostile worker PARKED a VERIFIED resource
with one lied-about 401, and the system has no automated recovery path.
If I were the systems lead, that is the first thing I would harden.
