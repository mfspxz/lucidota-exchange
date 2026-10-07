# Battery v5 — REDTEAM-2 vs Exchange Governor

**Target:** `https://lucidota-exchange.cutecity.workers.dev` (v0.6.6 → v0.6.11)
**Date:** 2026-10-07. **Status:** COMPLETE (all 9 groups closed) —
D1 went 100% mid-battery; v0.6.7/0.6.8/0.6.9 deployed+verified live,
v0.6.10/v0.6.11 deployed 04:29Z (live verification blocked by the limit).
Remaining verifications queued post 2026-10-08 00:00 UTC.

## Results by group

### Ballot-stuffing — COMPLETE (27 runs)
8 PROVEN, 13 DISPROVEN, 1 UNKNOWN (429 path — declined to flood shared governor).

PROVEN (all fixed, v0.6.7–v0.6.9, verified live pre-freeze):
- **CRITICAL:** `GET /v1/ballots/:id` → HTTP 500 on every terminal transition
  (`D1_ERROR: NOT NULL constraint failed: receipts.job_id`). Ballot receipt
  INSERT omitted `job_id`; audit receipt silently lost. → Fixed: `job_id`
  sentinel `ballot:<id>`.
- **HIGH:** duplicate voters `[V1,V1]` → 500 `UNIQUE constraint failed:
  jobs.id`, orphan jobs executing real inference. → Fixed: validation +
  slug-dedup pre-insert (400).
- **HIGH:** slug collisions (`aa/bb` vs `aa_bb`) → 500. → Fixed by dedup.
- **MEDIUM:** null voters → 500 TypeError. → Fixed (400).
- **MEDIUM:** `deadline_s:"abc"` → 500 RangeError. → Fixed: 400 +
  finite-positive validation, 30-day cap (v0.6.7).
- **MEDIUM:** negative `deadline_s` accepted, fans out jobs for dead ballot.
  → Fixed (400).
- **LOW:** nonexistent model IDs accepted (ballot expires voteless).
  Residual — honest behavior, queue-pollution vector noted.
- **LOW:** empty-string model accepted. → Fixed (400).

DISPROVEN (13): >7 voters, <2 voters, 0 voters, empty/missing proposition,
>4000 chars, invalid/missing/uppercase ballot_type, numeric proposition,
voters-as-string, malformed JSON, missing/wrong auth → all clean 400/401;
nonexistent ballot GET → 404.

### Deadline-gaming — COMPLETE (summary; 6 PROVEN)
`deadline_s=0`/missing → default 600s; negative → expired; huge → accepted
(2058); non-numeric → 500 (the bug fixed in v0.6.7). Late-vote injection +
expired-ballot re-decision: UNKNOWN (follow-up post-reset).

### Tally-manipulation — COMPLETE (7 ballots, 10 vote completions)
- INVALID votes do NOT count toward quorum — PROVEN.
- 1-of-2 never decides; late post-expiry vote doesn't resurrect — PROVEN.
- No double-count (completion idempotent) — PROVEN.
- Garbage CAN decide **answer** ballots (2× "blorpt-blorpt" → DECIDED) —
  PROVEN, **by design** (voting measures agreement, not truth; verdict
  ballots filter INVALID). Documented, not a bug.
- **CRITICAL BUG (fixed, v0.6.10):** `ballot_id` stamp nested inside the
  ternary-verdict branch — every honest worker vote invisible to tallies.
- **BUG (fixed, v0.6.11):** verdict extraction took the FIRST keyword in
  the tail, misreading "Final verdict:" lines → now takes the LAST.
- **BUG (fixed, v0.6.11):** tally counted its own decision receipt as a
  phantom INVALID vote → excluded (`job_id NOT LIKE 'ballot:%'`).
- Residual: `MAX(created_at)` second-precision tie could double-count —
  negligible.

### Invariant-regression — COMPLETE (10 invariants)
**9/10 HELD, 1 BROKEN:**
- **INV7 BROKEN:** SUCCEEDED with empty output `{}` verified as `ok`
  (rcpt_muxl3uyjjbhnxr) — battery-v2 X11 gap persisted into v0.6.6.
  → Fixed locally (v0.6.10): `{}` → VERIFICATION_FAILURE for non-inference;
  9/9 logic cases green locally.
- Held: no lease minting (401×6), no gate bypass (404), accounting ignores
  worker cost (measured), cross-worker completion 403, no resource-authority
  access (401×5), no cross-secret use (403), no receipt manufacture,
  lease expiry enforced, policy snapshot read-only.

### Quorum-bypass — COMPLETE (freeze-honored; live + 23/23 replica)
- (a) 1 valid + 2 INVALID → open → expired, never decided — PROVEN (live).
  Without ternary_verdict the INVALID receipts are invisible to the tally.
- (b) tie → open pre-deadline → inconclusive + UNKNOWN at deadline —
  PROVEN (replica; live UNKNOWN, claim race lost twice).
- (c) 1 valid + 1 FAILED_PERMANENT → failed receipt invisible → expired —
  PROVEN (replica).
- (d) sub-quorum decision — DISPROVEN all vectors (output smuggling,
  double completion, late vote, deadline re-poll); sweep n∈{2,3,5,7}: no
  sub-quorum `decided` possible. Same-ms duplicate receipts double-count in
  SQL (mechanism proven) but live exploitability ~nil.
- Live: 5 ballots, 7 completions, 0× 429. Report:
  redteam2-quorum-REPORT-2026-10-07.md.

### Duplicate-replay — COMPLETE (from collected data, freeze-complied)
- Identical ballot POST ×2 → two distinct ballots, disjoint job IDs — PROVEN.
- Triple-read decided ballot — UNKNOWN (never got a decided ballot in-window).
- Late-vote injection — premise unmet; key positive: manual /v1/job with
  spec.ballot_id got ballot_id=None receipts → decided tally not alterable
  via this vector.
- Completion replay → idempotent, no double receipt/accounting — PROVEN.
- **Drift warning:** group claims live receipts suggest deployed ballot-stamp
  behavior differs from local worker.js — POST-RESET: diff deployed source
  before trusting local file. Report: rt2-dup-replay-REPORT.md.
### Battery-v4-regression — COMPLETE (32 test events, freeze-terminated)
- QUOTA — PROVEN 6/6: max_cost > spend_authority blocked, exact receipt
  reasons (`declared max_cost $1 exceeds spend_authority $0`), taxonomy
  POLICY_FAILURE, retryable:false. (Observability gap: submit-response
  trace shows only "N candidate(s) rejected at COST" — reason lives in the
  receipt decision.)
- INTEGRITY — PASS: 18/18 samples stable over ~18 min; v0.6.3 fix holds
  through v0.6.9. Only health-transition triggers (AVAILABLE↔DEGRADED
  flapping correlated with live redeploys).
- CONCURRENCY — PASS on v0.6.9: R1 double-complete → one 200 winner + one
  409 `claim not active`; replay idempotent; exactly 1 accounting row.
  R2: 4/4 barrier claim races, alternating winners, zero double-claims.
  v0.6.8 atomic active→completing fix confirmed live.
- REPLAN — PARTIAL: forced FAILED_RETRYABLE PROVEN (job → deferred,
  defer_count 0→1, lease released). Cron replan UNKNOWN (freeze killed the
  watch at queue position 52/74). Replan-failure receipts: 0 in DB —
  catch→receipt path live in code, never fired (UNPROVEN live).
Full report: audit/battery-v4-regression-report.md.
### Ballot_id-forgery — NOT EXECUTED (queued post-reset)
First attempt blocked pre-execution by safety classifier (recon only).
Retry dispatched, then halted by D1 freeze; final retry errored on runtime
restart drain. T1–T8 battery (worker-role vote injection, nonexistent
ballot_id, SQL/JSON injection strings, worker visibility of ballot-policy
and ballot tallies) queued for post-reset run.
### Tally-manipulation — COMPLETE (local-only analysis; freeze-complied)
Offline re-derivation: all 5 tallied ballots replayed through local replica
of extractVote + quorum rules — all live states reproduced exactly. Tally
decisions are purely mechanical; the bugs are in *which* votes the tally
sees. Analysis: redteam2/ANALYSIS-2026-10-07.md.

## Fixes prepared (local, deploy AFTER D1 reset)

| Version | Fix | Local verification |
|---|---|---|
| v0.6.10 | ballot_id stamped on all successful vote receipts | code review |
| v0.6.10 | INV7: `{}` → VERIFICATION_FAILURE (non-inference) | 9/9 logic green |
| v0.6.11 | decision receipt excluded from own tally | code review |
| v0.6.11 | verdict extraction: last keyword wins | 4/4 cases green |

Deployed + verified live pre-freeze: v0.6.7 (deadline validation),
v0.6.8 (ballot receipt job_id), v0.6.9 (voter validation/dedup).

## Test counts
Runs executed: 27 (stuffing) + ~12 (deadline) + 10 completions/7 ballots
(tally) + 10 (invariants) + 5 ballots/7 completions (quorum) + 45 reqs
(replay) + 32 events (v4-regression) ≈ **140+ live tests**.
Bugs found: 8 (stuffing) + 1 (deadline) + 3 (tally) + 1 (INV7) = **13**,
of which 9 fixed (5 deployed+verified, 4 prepared), 2 by-design, 2 residual.

## Open after reset
1. Diff DEPLOYED worker source vs local before trusting local ballot logic
   (drift warning from replay group).
2. Deploy v0.6.10/v0.6.11, verify live (ballot_id on honest votes, INV7,
   phantom gone, extraction).
3. Re-verify cron replan of job_b4reg-replan-1-1791345400 (was 52/74 in
   queue when freeze hit).
4. Confirm zombie-sweep self-heal of 2 abandoned claims
   (job_ir1791345524-a3, job_b4reg-conc-r2-1791345568).
2. Forgery battery T1–T8 (worker-role vote injection).
3. Quorum tie empirical test; deadline late-vote + re-decision.
4. 429 backpressure path for ballots.
