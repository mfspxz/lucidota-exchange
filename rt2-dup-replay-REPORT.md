# REDTEAM-2 duplicate-replay — findings report (frozen mid-run)

**Target:** live Exchange governor v0.6.6, `lucidota-exchange.cutecity.workers.dev`
**Window:** 2026-10-07 03:48–04:16 UTC. **Stopped by D1 EMERGENCY FREEZE** (Zackary order:
zero live-fire until 2026-10-08 00:00 UTC; account at 87% of 5M rows_read/day).
**Method:** real User-Agent, ~1 req/1.7s, zero 429s hit (~45 requests total).
**Artifacts:** `rt2-dup-replay.py`, `rt2-dup-replay-log.json` (this dir). Ledger seq 461.

## Per-test verdicts

### (a) Identical POST /v1/ballots ×2 — PROVEN
Two distinct ballots: `bal_muxkio12es1rfs`, `bal_muxkiuzcmhfd5n`.
Job IDs ballot-scoped (`job_ballot_<bid>_<model_slug>`), disjoint sets, no cross-ballot
job collision. Ballot submission is intentionally non-idempotent; job identity is.

### (b) Triple-read of decided ballot — UNKNOWN (not completed)
Never obtained a decided ballot in-window (claim race lost to swarm; ballot stayed open,
then freeze hit). Triple GET on open `bal_muxl7degm8jycg` returned 3 different
full-response hashes — **methodology artifact**: response carries a live `at` timestamp.
Tally payloads were byte-identical across reads. Redo must compare the `tally` object only.
Secondary: expired ballots returned stable state/verdict tallies across repeated polls.

### (c) Late vote job on decided ballot — NOT COMPLETED as specified; strong partial
Premise unmet: target ballot was still OPEN. But the injection attempt yielded a
critical positive finding:
- Manual `POST /v1/job` with `spec.ballot_id=<bid>` + idempotency key
  `ballot_<bid>_rt2_latevote` (matching the vote-job key pattern) → job created as
  `job_ballot_bal_muxl7degm8jycg_rt2_latevote`; resubmission → `{deduplicated:true}`.
- **Both receipts for the injected job (deferred `rcpt_muxlfewdckdv5r`, terminal
  `rcpt_muxlfx8o382qqa`) have `ballot_id=None`.** The tally never saw the vote
  (`votes:[]` before and after). The governor's ballot linkage is stricter than
  `spec.ballot_id` + key pattern — this injection vector cannot reach a tally,
  decided or otherwise. Exact stamp condition UNKNOWN (spec-fetch aborted; frozen).
- Net for the security question: decided-tally alteration via manual job injection
  is **not demonstrated**; the receipt never joins the ballot's receipt set.

### (d) Completion replay, same claim, on a vote job — PROVEN
- 1st `POST /v1/complete` → receipt `rcpt_muxlfx8o382qqa`, result ok.
- 2nd identical → HTTP 200 `{idempotent:true, note:"already completed — no duplicate accounting"}`.
- `/v1/receipts?job=` → exactly 2 rows (1 deferred + 1 terminal). **No double receipt,
  no double accounting.** (Tally double-count N/A — vote wasn't ballot-linked; the
  receipt-level W2 idempotency is what's proven.)

## Secondary findings
1. **Source/deploy drift (actionable):** local `worker.js` nests the `ballot_id` receipt
   stamp inside the `ternary_verdict` block (~L786-791) — if deployed, no ballot could
   ever tally (nim-claimant never emits ternary_verdict). LIVE receipts prove the
   deployed governor stamps `ballot_id` unconditionally (coordinator ballot's votes:
   `ballot_id` set, `ternary_verdict=None`). **Do not treat local worker.js as the
   deployed source for ballot behavior.** Diff deployed source before next battery.
2. **Tally semantics (code + live consistent):** tally recomputed from receipts
   (latest per `job_id`) on every GET; `state`/`verdict` transition only when
   `state=="open"` → decided ballots' decisions are frozen on read.
3. **Trust-class self-attestation:** `/v1/workers/register` accepted
   `trust_class:"local"` from the Exchange key alone (`rt2-local`, no proof of
   credential-holding). The nim-nvidia local-trust gate is only as strong as
   Exchange-key custody. By design (key = governor admin), but the label over-claims.
4. **Claim mechanics:** `claim_id` nests at `delegation.claim_id`; claim is
   oldest-lease-first per resource — no job targeting. In a busy queue you will claim
   others' jobs (observed: `nim-claimant-01`, sibling `rt2-forgery-01` draining).
5. `/v1/job` idempotency confirmed live: same key → `{deduplicated:true, same job_id}`.

## Post-freeze redo list (after 2026-10-08 00:00 UTC)
- (b): decided ballot, tally-only comparison (strip `at`), 3+ reads.
- (c): decided ballot first; determine exact `ballot_id` stamp condition
  (fetch stored spec of injected job; vary `input.model`, voter membership).
- (d): replay on a ballot-*linked* vote job; confirm tally unchanged.
- Reconcile local worker.js vs deployed source (finding 1).
