# v0.7.0 Exchange Stage — Code Review

**Reviewed:** 2026-10-07 ~09:40 PDT by lucidota (coordinator-exchange subagent)
**Files:** `~/workspace/exchange/worker.js` (live, v0.6.11, mtime Oct 7 06:28) vs `~/workspace/exchange/worker.js.exch-stage` (staged, mtime Oct 7 04:29)
**Method:** full `diff` (249 diff lines, 205 changed lines), auth-gating trace, schema/function reference check.
**Constraints honored:** no live probes against the Exchange, no deploys. Static review only.

## Verdict

**DO NOT DEPLOY the stage file as-is.** It is stale relative to the live worker: it predates the D1 read-burn fix (isolate-level migrate guard, seq 855) and would regress it. The version string's claim "rebased on 0.6.11" is not accurate against the current live file. Re-rebase onto current `worker.js` first, then address the D1-cost flags below.

Note: the version string names **9** items (exch-001/003/004/006/008/011/021/025/026); the ledger (seq 708) said "8 items." Count discrepancy is minor but recorded.

## The 9 staged items

### exch-001 — Ballot idempotency_key replay protection
Resubmitting `POST /v1/ballots` with the same caller-chosen `idempotency_key` (≤128 chars) returns the recorded ballot + tally instead of re-executing. Schema: `ballots.idempotency_key TEXT` via `ensure()`.
**Risk (minor):** no UNIQUE constraint on the column — two concurrent submits with the same key can both pass the `SELECT`-then-`INSERT` check and create duplicates. Low concurrency makes this unlikely, but the replay guarantee is not race-safe. Recommend a unique index (D1 supports it) or accept the documented limitation.

### exch-003 — ballot_id stripped from user /v1/job specs (forgery rejection)
User-supplied `spec.ballot_id` is deleted at the submit boundary and logged to `evidence_log` (not silent). Only `POST /v1/ballots` → `runJob` can set it.
**Risk:** none found. This is a clean security fix. The log's `resource_id` ("job:pending") is slightly odd but harmless.

### exch-004 — POST /v1/ballot-policy (quorum policy admin)
Updates default voter set (2–7 model ids, slug-deduped), `deadline_s` (finite, positive, 30d cap), `tie_policy` (enum). Existing ballots keep their snapshot; applies to future ballots only; workers unaffected. Change logged to `evidence_log`.
**Risk:** none found. Validation is solid; route sits behind the Exchange master-key gate (verified at handle() line ~1980). Auth: OK.

### exch-006 — INTEGRITY fires logged to evidence_log
Every INTEGRITY gate fire now writes full gate inputs (shortlist vs fresh state/evidence, adapter claim) to `evidence_log`.
**Risk (D1 cost):** this is a new write on every INTEGRITY fire, including cron cycles. Under the current D1 budget crisis this adds write pressure with no sampling or rate limit. Recommend: sample, or gate behind a verbosity flag.

### exch-008 — CONTESTED receipt marker + tally exclusion
`receipts.contested` column (0|1); `POST /v1/receipts/:id/contest` sets it (reversible via `uncontest:true`); contested receipts excluded from ballot tallies (evidence retained, exclusion reversible). Both tally queries filter `(contested IS NULL OR contested=0)`.
**Risk:** none found. Auth behind master key (verified). Design is honest (marker-as-evidence, not deletion).

### exch-011 — /v1/receipts pagination
`limit` (default 50, max 500) + `offset`, plus `total` count in the response.
**Risk (D1 cost, moderate):** the `total` is a `SELECT COUNT(*)` full scan of receipts on every call. On a large receipts table under free-tier D1 this is expensive. Recommend: make `total` opt-in (`?total=1`) or drop it.

### exch-021 — Weighted voting by measured reliability
Each vote weighted by the model's verify pass rate (`ok/total` terminal receipts via `json_extract(decision,'$.vote_model')`), discounted `1/sqrt(domain group size)` for correlated voters sharing a provider-domain failure mode. Defaults to 1.0 ("no-data" basis). Quorum stays participation-based; weights decide the verdict. `counts_raw` preserved alongside `counts_weighted` in the tally — good auditability.
**Risk (D1 cost, HIGH — the most expensive new feature):** one aggregate query per distinct voter model per tally, each a `json_extract` scan over receipts with no usable index. This is N+1 full-ish scans on the hottest path (tally). Under the D1 budget this is the item most likely to re-trigger exhaustion. Recommend: precompute per-model reliability into a small table refreshed by cron (5-min cadence already exists), and have tallyBallot read that table instead.

### exch-025 — Cron replan receipts for all outcomes
Every cron replan now writes a receipt (`replanned`, trigger + outcome); previously only failures were distinguishable from fresh submissions.
**Risk (D1 cost, low-moderate):** one extra write per replan per cron tick. Bounded (replans are bounded per the cron design). Acceptable, but it does add steady write load — include in the probe-budget accounting.

### exch-026 — Per-resource backpressure thresholds
`QUEUE_THRESHOLDS = { "nim-nvidia": 36 }`, default 40, justified from measured drain rates (d1-mirror: 450 completions/6.47h = 1.16/min; 36 ≈ 31min buffer, matching REDTEAM-2's validated 35–38 depth).
**Risk (minor logic smell):** the ballot-submit fan-out check hardcodes `queueThreshold("nim-nvidia")` for ALL voter sets regardless of which resource the voters execute on. Should use the relevant resource's threshold (or the default) instead of nim-nvidia's. Also the thresholds map has exactly one entry — fine as a start, but the "measured" justification lives only in a comment; no mechanism keeps it fresh as drain rates change.

## Cross-cutting risks

1. **STALENESS (deploy-blocker):** stage mtime 04:29 < live worker 06:28. The stage lacks the isolate-level `__migrated` guard (seq 855). Deploying it reverts `migrate(db)` to every-request — exactly the read-burn the guard was built to stop. **Re-rebase required.**
2. **D1 budget blindness:** exch-006, exch-011 (COUNT(*)), exch-021 (N+1 scans), exch-025 all add D1 reads/writes with no budget awareness. The stage was written before the 100% exhaustion was fully internalized. Every one of these needs a cost estimate before deploy — see `probe-budget-design.md`.
3. **Test evidence:** "implemented+tested locally" (seq 708) is [STATED]. No test file exercising the stage items was found (BUILD-RECORD.md has no v0.7.0 section; no battery covers idempotency/weighted-voting/contest). Treat as untested until a battery runs against it.
4. **Auth:** verified — all new mutating routes (`/v1/ballot-policy` POST, `/v1/receipts/:id/contest` POST) sit behind the Exchange master-key gate. No bypass introduced. The worker-protocol routes intentionally precede the gate (documented, pre-existing).
5. **Schema migration:** new columns via `ensure()` (idempotent `ALTER TABLE ADD COLUMN` pattern) — safe for deploy; no destructive migration.

## Recommended deploy sequence (when D1 recovers)

1. Re-rebase stage onto current live `worker.js` (recover the `__migrated` guard).
2. Add UNIQUE index on `ballots.idempotency_key` (or document the race).
3. Make exch-011 `total` opt-in.
4. Replace exch-021's per-tally reliability queries with a cron-refreshed table (or defer exch-021 to v0.7.1).
5. Sample or flag-gate exch-006 evidence_log writes.
6. Fix the hardcoded `queueThreshold("nim-nvidia")` in ballot fan-out.
7. Run a targeted battery (idempotency replay, contest/uncontest tally exclusion, weighted tally vs raw, policy update isolation) before deploy.
