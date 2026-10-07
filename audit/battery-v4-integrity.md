# INTEGRITY-GROUP — battery v4 live verification (governor v0.6.3)

**Date:** 2026-10-07 03:33–03:55 UTC (Tue Oct 6 20:33–20:55 PDT)
**Target:** live Exchange governor https://lucidota-exchange.cutecity.workers.dev (v0.6.3)
**Script:** [worker-battery-v4-integrity.py](../worker-battery-v4-integrity.py) · log `/tmp/integrity-battery-v4.log` · raw `/tmp/integrity-battery-v4.jsonl`
**Verdict: PASS** — zero INTEGRITY rejections across 10 jobs + 10 rolling-window receipt scans; COST gate verified.

## 1. Fix under test (read in `~/workspace/exchange/worker.js`)

v0.6.3, INTEGRITY gate (lines ~1259–1273): when a candidate's last observation is stale
(age > `staleAfterEv`), the governor previously ran a revalidation probe via the
candidate's adapter. For **claimed** adapters the probe always fails — the Worker
holds no credential — writing a false failure that demoted `nim-nvidia` and failed
every subsequent job at INTEGRITY until a claimant re-promoted it (the "non-stationary"
boundary the probe harness saw moving 02:35–03:05Z). The fix skips the probe for
`adapter.claimed`:

```js
if (ageMin > staleAfterEv(fresh.evidence_state)) {
  if (adapter.claimed) {
    // Claimed resources are observed by their claimant, never by the Worker.
    // The claimed probe ALWAYS fails (no credential held) — running it here
    // writes a false failure that demotes the resource and fails every
    // subsequent job at INTEGRITY until a claimant re-promotes it (battery v4:
    // this was the "non-stationary" INTEGRITY boundary). Skip the probe;
    // staleness demotion is the cron's job via demoteCheck.
  } else {
    let p; try { p = await adapter.probe(r); } catch ...
    ...
```

Code presence: **confirmed**. `const VERSION = "0.6.3"` line 72.

## 2. Live run: 10 inference jobs, 2-min cadence, max_cost=0

Idempotency keys `b4-int-1` … `b4-int-10`, capability=inference, data_class=public,
tiny inputs (4 max_tokens). Every job: HTTP 200, state=`deferred`,
zero INTEGRITY rejections at submit and in receipts.

`state=deferred` is the **expected** outcome, not a failure: nim-nvidia is a claimed
resource (credential off-Worker), so the governor delegates (`delegateExecution`,
lines 520–545), mints a lease with a 10-min pickup window, and parks the job as
`deferred` until the nim-claimant hook picks it up. Verified live on `job_b4-int-1`
(`/v1/job/job_b4-int-1` receipt `rcpt_muxjzgos34aikp`): full gate trace shows
nim-nvidia passed IDENTITY, AUTHORITY, DATA, EVIDENCE, QUOTA, COST **and INTEGRITY**
(`INTEGRITY pass=true, "1 candidate(s) passed INTEGRITY"`), selected by EFFICIENCY
(score 0.955), lease `lease_muxjzgdiuw6x9y` delegated. The 4 other inference lanes
(lane-kilo, lane-llm7, lane-ovh, lane-pollinations) were refused at EVIDENCE
(`evidence_state=UNKNOWN — not verified usable capacity`) — unrelated to this battery.

nim-nvidia resource state at run start (via `/v1/resources`): `AVAILABLE` / `VERIFIED`.

| job | key | http | state | int_reject_submit | int_fail_receipts | verdict |
|---|---|---|---|---|---|---|
| 1 | b4-int-1 | 200 | deferred | 0 | 0 | PASS |
| 2 | b4-int-2 | 200 | deferred | 0 | 0 | PASS |
| 3 | b4-int-3 | 200 | deferred | 0 | 0 | PASS |
| 4 | b4-int-4 | 200 | deferred | 0 | 0 | PASS |
| 5 | b4-int-5 | 200 | deferred | 0 | 0 | PASS |
| 6 | b4-int-6 | 200 | deferred | 0 | 0 | PASS |
| 7 | b4-int-7 | 200 | deferred | 0 | 0 | PASS |
| 8 | b4-int-8 | 200 | deferred | 0 | 0 | PASS |
| 9 | b4-int-9 | 200 | deferred | 0 | 0 | PASS |
| 10 | b4-int-10 | 200 | deferred | 0 | 0 | PASS |

## 3. Rolling-window cross-check

`GET /v1/receipts` (latest 50) scanned 10× across the 20-min window; each receipt's
`decision` trace and `rejected` list parsed for gate=INTEGRITY failures.
**Result: 0 INTEGRITY failures in all 10 scans.** No resource-state capture needed
(no failure occurred). The gate stayed stable across repeated claimant observation
cycles — no boundary movement.

## 4. COST gate (QUOTA-gap) check: max_cost=1, no spend_authority

`POST /v1/job` with `max_cost: 1`, idempotency `b4-cost-probe` →
HTTP 200, state=`blocked`, nim-nvidia rejected at **COST**:
`"declared max_cost $1 exceeds spend_authority $0"` (taxonomy `POLICY_FAILURE`,
retryable=false). **PASS** — exact required reason string. Fails closed as specified.
(The other 4 lanes rejected at EVIDENCE as before.)

## 5. Honest gaps

- **The stale-observation skip branch was code-verified, not live-triggered.**
  nim-claimant's 120s observation cycle keeps nim-nvidia's observation age (~2–3 min)
  well under `staleAfterEv(VERIFIED)=15min`, so the `ageMin > staleAfterEv` branch
  never fired during the run. Forcing staleness would require writing directly to
  `health_observations` in D1 — internal state manipulation, out of scope for a
  live-behavior battery. If a future battery needs to exercise the skip path,
  that's the way to do it (or wait 15+ min with the claimant hook stopped).
- Jobs were delegated, not executed inline; final claimant completion receipts were
  outside the battery's scope (INTEGRITY gating happens at submit/delegate time).
- No 429s were encountered, so the backoff path went unexercised. Total spend: **$0**
  (max_cost=0 on all battery jobs; cost probe was blocked before any lease).

## Artifacts

- Battery script: `~/workspace/exchange/worker-battery-v4-integrity.py`
- Raw per-job records: `/tmp/integrity-battery-v4.jsonl`
- Summary JSON: `/tmp/integrity-battery-v4-summary.json`
- Full log: `/tmp/integrity-battery-v4.log`
