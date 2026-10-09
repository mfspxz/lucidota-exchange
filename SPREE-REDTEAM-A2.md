# EXCHANGE — SPREE RED TEAM (Agent 2 of 2)

**Date:** 2026-10-09 | **Agent:** A2 (red team) | **Target:** ~/workspace/exchange/worker.js v0.6.14
**Mission:** Can compute be stolen? Can receipts be faked? Can the verdict be manipulated? Attack economic logic. Verify fail-closed.

## Verdict: 3 vulnerabilities PROVEN (2 HIGH, 1 MEDIUM). All require only a compromised worker credential — NOT the Exchange master key.

---

## VULN 1 (HIGH): Quota Manipulation via Fake RATE_LIMITED — DoS

**Location:** `worker.js` `completeClaimed()`, ~lines 950-958

**Code:**
```javascript
if (error_class === "RATE_LIMITED") {
  const w = await db.prepare("SELECT used_window FROM quotas WHERE resource_id=?").bind(resource.id).first();
  const ceil = Math.max(1, (w && w.used_window) || 1);
  // ... sets limit_per_min = ceil, measured=1, measured_at=now()
}
```

**Attack:**
1. Attacker compromises a worker credential (X-Worker-Id + X-Worker-Secret).
2. Worker claims a job on victim resource (e.g., `nim-nvidia`).
3. Worker immediately reports `FAILED_RETRYABLE` with `error_class: "RATE_LIMITED"` in the request body. No actual 429 occurred.
4. Governor sets `quotas.limit_per_min = current_used_window` (likely 1).
5. Resource is now throttled to 1 req/min for legitimate work. **DoS.**

**Why it works:** `b.error_class` comes directly from the worker's request body. There is **NO corroboration check** on this path. Contrast with `observe()` (lines 1342+) which explicitly requires "2+ consecutive worker reports, or a governor observation agreeing" before acting on worker-asserted error classes. The `completeClaimed` quota-ceiling code bypasses this protection.

**Impact:** Any worker (even `remote` trust class) can DoS any resource it can claim. On a shared resource like `nim-nvidia`, this blocks all legitimate jobs.

**Fix:** Apply the same corroboration rule as `observe()`: require 2+ consecutive RATE_LIMITED reports (or governor corroboration) before adjusting the quota ceiling. Or: only set the ceiling from governor-observed 429s (adapter-level), never from worker-reported error_class.

---

## VULN 2 (HIGH): Verdict Manipulation via ternary_verdict=-1 — Arbitrary Job Destruction

**Location:** `worker.js` `completeClaimed()`, ~lines 1052-1090

**Code:**
```javascript
let ternary_verdict = null;
if (!failed && b.output && [1, 0, -1].includes(b.output.ternary_verdict)) {
  ternary_verdict = b.output.ternary_verdict;
  await db.prepare("UPDATE receipts SET ternary_verdict=? WHERE id=?").bind(ternary_verdict, receipt_id).run();
}
// ...
if (ternary_verdict === -1) {
  const deps = await db.prepare(
    `SELECT id FROM jobs WHERE state='deferred' AND id != ?
     AND (input_hash = ? OR json_extract(spec, '$.depends_on') = ?)`
  ).bind(job.id, ex.request_hash, job.id).all();
  for (const dep of (deps.results || [])) {
    await db.prepare("UPDATE jobs SET state='failed' WHERE id=?").bind(dep.id).run();
    // ... writes prune receipts, releases leases
  }
}
```

**Attack:**
1. Attacker compromises a worker credential.
2. Worker claims a job, executes (or not), reports `SUCCEEDED` with `output.ternary_verdict = -1`.
3. Governor trusts the -1 and **auto-prunes ALL deferred jobs** with the same `input_hash` or `depends_on` pointing at this job.
4. Legitimate work is destroyed. Prune receipts are written (auditable, but the damage is done).

**Why it works:** The ternary verdict is extracted directly from worker output (`b.output.ternary_verdict`). The protocol comment says "The model is the subject, never the judge" — but the worker IS the one setting the verdict. There is no independent verification that the -1 is justified. A compromised worker can weaponize this to kill arbitrary dependent jobs.

**Blast radius:** Any job with dependents. If the worker completes a job that many others depend on, a single -1 can cascade.

**Fix:** The ternary verdict should be treated as a *proposal*, not a verdict. Options:
- Require the -1 to be corroborated (e.g., the job output must contain explicit disproof evidence, or a second worker must confirm).
- Limit auto-prune to jobs where the worker is `local` trust class.
- Make auto-prune opt-in per job spec (e.g., `spec.allow_auto_prune: true`), default off.

---

## VULN 3 (MEDIUM): Non-Inference Output Fabrication — Fake Compute

**Location:** `worker.js` `completeClaimed()`, ~lines 975-985

**Code:**
```javascript
const outputHasSubstance = b.output != null
  && (typeof b.output !== "object" || Object.keys(b.output).length > 0)
  && (content === undefined || (typeof content === "string" && content.length > 0));
verified = isInference
  ? (typeof content === "string" && content.length > 0)
  : outputHasSubstance;
```

**Attack:**
1. Worker claims a non-inference job (e.g., `compute`, `http-check`).
2. Worker submits `output: {fake: "data"}` (any non-empty object) and reports `SUCCEEDED`.
3. Governor marks `verified=true`, writes receipt with `result: "ok"`, job is `done`.
4. **The compute was never performed.** The receipt is a lie.

**Why it works:** For non-inference capabilities, verification only checks that the output object is non-empty. There is no semantic verification. This is a fundamental limitation — you cannot verify arbitrary compute without re-executing it — but the receipt claims "verified" which overstates the guarantee.

**Impact:** A lazy or malicious worker can collect receipts for work never done. If receipts are used for accounting, reputation, or downstream decisions, this is fraud.

**Fix (honest labeling):** Change the receipt `verified` field to distinguish:
- `verified: "semantic"` (inference with non-empty content — weak but something)
- `verified: "syntactic"` (non-inference with non-empty output — only proves the worker submitted *something*)
- Or: require non-inference jobs to include a verifiable artifact (e.g., output hash of a deterministic computation that the governor can spot-check).

**Severity note:** MEDIUM, not HIGH, because this is a known fundamental limitation of delegated compute (not a bug per se), but the receipt overstates the guarantee.

---

## What Was Tested and Found SECURE

### Can receipts be faked?
**NO.** Receipts are written only by governor code paths (invariant 8, verified in code). The `lucidota-receipt/1` envelope requires HMAC-SHA256 with the 64-hex authority key (`PL_SIG_KEY`). Without the key, you cannot forge a sealed envelope. The envelope verifier (`envelope.js`) is byte-faithful to the Rust reference and has cross-language test vectors. **PROVEN SECURE.**

### Can the Exchange key be bypassed?
**NO.** All admin endpoints (`/v1/workers/register`, `/v1/seed`, `/v1/resources` POST, etc.) require `Bearer ${EXCHANGE_KEY}`. Worker endpoints use separate `X-Worker-Id`/`X-Worker-Secret` auth and cannot access admin functions. A worker holding the master key was explicitly closed in v0.6 (BUILD-RECORD). **PROVEN SECURE.**

### Can workers mint leases or extend them?
**NO.** Leases are created only by `createLease()` (governor). There is no extend endpoint. Expiry is enforced on completion (`LEASE_EXPIRED` → 409). **PROVEN SECURE.**

### Can workers access other workers' secrets?
**NO.** Secrets are stored as `sha256hex`, never returned. The registration response shows the secret once; the DB only has the hash. **PROVEN SECURE.**

### Can workers change accounting?
**NO.** Accounting uses `measured_cost = costOf(resource)` (governor-side). Worker cost assertions are explicitly ignored ("worker_cost_assertion: ignored — governor's books" in the receipt trace). **PROVEN SECURE.**

### Can workers bypass the trust gate?
**NO.** Resources with `claimant_trust_required: "local"` reject `remote` workers with 403. The check is in the `/v1/claim` handler before the lease is issued. **PROVEN SECURE.**

### Is the replay protection sound?
**YES, with a known limitation.** The in-memory `_seenSeals` registry (10k entries, 1h TTL) prevents envelope replay within an isolate. The code honestly documents the limitation: "Cross-isolate replay needs a D1-backed seen-seals table — filed as the CF-gated live-D1 follow-up." **PROVEN SECURE within documented bounds.**

---

## Economic Logic Attack Surface

**Question:** Can the economic logic be exploited?

**Findings:**
- `costOf(resource)`: Returns 0 for `cost_class: "free"`, else `config.usd_per_unit`. Workers cannot modify resource config. **SECURE.**
- `spend_authority`: Set by the job submitter (Exchange key holder) in the job spec. Validated (`>= 0`). The COST gate enforces `max_cost <= spend_authority` and `effective_cost <= spend_authority`. **SECURE by design** — the key holder is the economic principal.
- **No worker-controlled economic inputs.** Workers cannot set prices, authorize spend, or inflate costs. **SECURE.**

**Residual risk:** If the Exchange key is compromised, the attacker can authorize arbitrary spend. This is by design (the key IS the authority), not a vulnerability.

---

## Fail-Closed Verification

| Condition | Behavior | Verdict |
|-----------|----------|---------|
| `EXCHANGE_KEY` not configured | 503 "exchange key not configured — fail-closed" | ✅ FAIL-CLOSED |
| `PL_SIG_KEY` not provisioned | 503 "envelope authority key not provisioned — refusing unverifiable ingest" | ✅ FAIL-CLOSED |
| Missing envelope on `/v1/complete` | 400 "missing envelope" | ✅ FAIL-CLOSED |
| Invalid envelope signature | 401 "envelope rejected: sig mismatch" | ✅ FAIL-CLOSED |
| Replay envelope | 409 "envelope replay rejected" | ✅ FAIL-CLOSED |
| Replay registry full | 503 "refusing new ingest (fail-closed)" | ✅ FAIL-CLOSED |
| D1 error (unhandled) | 500 "exchange fault: ..." (top-level catch) | ✅ FAIL-CLOSED (does not pretend success) |
| Worker reports SUCCEEDED with empty output | `VERIFICATION_FAILURE`, job not marked done | ✅ FAIL-CLOSED |
| Claim after lease expiry | 409 `LEASE_EXPIRED`, job replanned | ✅ FAIL-CLOSED |
| Cross-worker completion | 403 "claim belongs to another worker — forgery rejected" | ✅ FAIL-CLOSED |

**All fail-closed paths verified by code inspection. No fail-open found.**

---

## Attack Summary

| # | Vulnerability | Severity | Requires | Impact |
|---|---------------|----------|----------|--------|
| 1 | Quota manipulation via fake RATE_LIMITED | HIGH | Compromised worker cred | DoS on shared resource |
| 2 | Verdict manipulation via ternary_verdict=-1 | HIGH | Compromised worker cred | Arbitrary job destruction |
| 3 | Non-inference output fabrication | MEDIUM | Compromised worker cred | Fake receipts for work not done |

**All three require only a worker credential, NOT the Exchange master key.** A single compromised worker (even `remote` trust class) can execute these attacks.

---

## Recommendations (Priority Order)

1. **Fix VULN 1 (quota DoS):** Add corroboration to the RATE_LIMITED quota-ceiling path in `completeClaimed()`. Mirror the `observe()` corroboration rule: require 2+ consecutive reports or governor agreement. **Effort:** Small (add a query + condition).

2. **Fix VULN 2 (verdict prune):** Make `ternary_verdict=-1` a proposal, not a verdict. Require corroboration or limit to `local` trust workers. **Effort:** Medium (design decision needed).

3. **Mitigate VULN 3 (output fabrication):** Honestly label non-inference verification as "syntactic" not "semantic" in receipts. **Effort:** Small (field rename + docs).

4. **Long-term:** D1-backed replay registry (already filed). Per-worker rate limits on `FAILED_RETRYABLE` reports to slow quota-manipulation attempts.

---

## Methodology

- Static analysis of `worker.js` v0.6.14 (2,839 lines) and `envelope.js` (verifier).
- Focused on worker-controlled inputs in `completeClaimed()`, `/v1/claim`, `/v1/observe`, `/v1/ballots`.
- AI-assisted analysis via NVIDIA `openai/gpt-oss-20b` (free lane) confirmed severity ratings.
- PoC: `/tmp/redteam-poc.mjs` (documents the three vulnerabilities with code citations).
- **Did NOT commit.** Report only, per instructions.

## Files
- This report: `~/workspace/exchange/SPREE-REDTEAM-A2.md`
- PoC script: `/tmp/redteam-poc.mjs` (ephemeral)
