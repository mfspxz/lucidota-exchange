# Audit receipt — exchange security hardening, 2026-10-10

Revision-bound record of the external-review fix campaign
(external deep source review: 7.4/10 overall — 8.5 architecture,
6.5 security boundary, 8/10 evidence & testing, 6/10 production confidence).

## Revision

- Base (pre-fix): `978eacec7189b5116490f9e323c065dd8fb0e5d0` (worker.js v0.6.15)
- Fix commits (master, in order):
  - `7380e04b3420756555ed51a1840f7844a9271f07` — P1 distinct-eligible-worker disproof corroboration
  - `86209ffc3c84917e8b10d9fca6e3a5d781db979e` — P1 outcome validation before active→completing flip
  - `0a290a72cf2a03f00a6beee1a76bd1a92f9cd5a9` — P1/P2 durable D1 replay registry
  - `aeae5f0117083d7767f41a32c045c7f86c9b4958` — VERIFY cron drift (docs)
  - `252a4094c515cd0ae719a722fe1faa5c82bac612` — syntax fix (block comment)
- Post-fix: worker.js v0.6.16 (this receipt's test revision)
- No push performed (hardening-only order). No visibility change.

## Test method (all on the CURRENT revision, not historical reports)

- Harness: `tests/hardening.test.mjs` drives the REAL worker.js against
  `tests/d1-shim.mjs` (node:sqlite D1 facade). Two module isolates via
  `import("../worker.js?iso=a|b")` share one D1 for cross-isolate tests.
- Exact command: `node tests/hardening.test.mjs` (node v24.20.0)
- Raw output: `audit/hardening-run-20261010.log`
- Regression: `node tests/ingest-envelope.test.mjs` →
  `audit/envelope-run-20261010.log`
- Pre-fix red run (base 978eace): 13 passed, 5 failed —
  T1d (same-worker double -1 pruned), T2b/T2c (claim stranded in
  'completing'), T4b/T4c (cross-isolate replay accepted, concurrent race
  both 200). Every finding reproduced before its fix.
- Post-fix (v0.6.16): **18 passed, 0 failed**; envelope suite 26/26 green.

## Finding 1 — P1: disproof corroboration didn't establish independent witnesses

- Defect (confirmed on base): `-1` auto-prune counted proposals
  (`COUNT(*)` over disproof_proposal receipts). One worker submitting two
  -1 completions for the same input_hash reached votes=2 alone and pruned
  legitimate deferred work. Test T1d reproduced: jB pruned after w1's
  second -1.
- Fix (7380e04): corroboration = `COUNT(DISTINCT c.worker_id)` over
  proposals, joined receipts→claims→workers with
  `COALESCE(workers.disabled,0)=0` (eligible = registered, not disabled).
  The current worker's unstamped proposal adds one vote only if it has not
  already proposed for this input_hash. Proposal→prune runs as one D1
  batch (atomic on D1); dep updates conditional on `state='deferred'`;
  prune receipts use deterministic ids `rcpt-prune-<dep>-<job>` with
  `INSERT .. ON CONFLICT DO NOTHING` — a raced double-prune cannot
  double-account.
- Residual race (documented in code): two -1 completions that both read
  votes<2 before either stamps both record proposals and neither prunes —
  fail-safe (no prune); the next -1 completes the quorum.
- Evidence: T1a–T1h green — single witness no prune; hostile same-worker
  twice no prune; two distinct workers prune; exactly one prune receipt.

## Finding 2 — P1: malformed completion could strand a claim

- Defect (confirmed on base): `completeClaimed` flipped active→completing
  BEFORE validating `worker_outcome`; a bogus value 400'd with the claim
  already in 'completing' — uncompletable until the 15-min stuck sweep,
  and the worker's retry 409'd. Tests T2b/T2c reproduced.
- Fix (86209ff): `worker_outcome` validated against WORKER_OUTCOMES
  BEFORE the atomic flip. No state transition precedes validation.
- Crash recovery: the 15-min completing→active reset in
  `sweepExpiredClaims` (pre-existing, reachable via cron scheduled() and
  `POST /v1/admin/sweep-expired-claims`) remains as the crash backstop —
  the reviewer missed it; verified present and tested (T3a–T3c green).
- Evidence: T2a–T2c green — 400 leaves claim 'active'; claim completes
  normally afterwards.

## Finding 3 — P1/P2: replay protection per-isolate, not globally durable

- Defect (confirmed on base): `_seenSeals` was isolate-local memory; the
  same sealed envelope submitted to a second isolate returned 200.
  Tests T4b/T4c reproduced (both isolates 200; concurrent race 200,200).
- Fix (0a290a7): D1 `seal_registry(seal PK, first_seen)`; the arbiter is one
  atomic `INSERT .. ON CONFLICT(seal) DO NOTHING` per ingest (changes=0 ⇒
  409 replay). Isolate memory stays as a read-free fast path. D1-budget
  design: single PK write per ingest (a write, not a read — 100K/day free
  tier), zero D1 reads on the hot path; bounded retention
  (`pruneSealRegistry`, one capped 500-row DELETE per cron run, in
  scheduled()). D1 unreachable ⇒ 503 fail-closed, never fail-open.
- Evidence: T4a–T4d green — cross-isolate replay 409; concurrent
  same-seal race exactly one 200 + one 409; distinct envelope still 200.

## Finding 4 — VERIFY: cron drift

- Read-only Cloudflare API check (schedules endpoint, via skill tooling):
  deployed = `*/30 * * * *`, created+modified 2026-10-09T16:24:01Z.
- Verdict: **no deployment drift**. wrangler.toml (`*/30`) == deployed.
  The */5→*/30 throttle (commit 64b31c1, 5.3M D1 reads/day vs 5M quota)
  is the deployed state. The drift was stale comments only ("Runs every
  5 min", "5-min cron") — fixed in worker.js; BUILD-RECORD.md got a dated
  amendment (history preserved, not rewritten).
- Intended cadence: */30 (burn-safe per the standing D1-leak directive).
  The replan/recovery loop (zombie sweep incl. stuck-'completing'
  recovery, orphan reaper, bounded replan 2/run) is verified present in
  scheduled() at that cadence.

## Preserved (per reviewer's preserve list)

- Governor/worker authority split + frozen Worker Protocol v1 — untouched.
- Atomic completion idempotency (battery v4 R1 W2) — untouched; T2
  exercises it alongside the new validation order.
- Execution/receipt/accounting distinction — untouched.
- Honest unknown-capacity / retryable-failure treatment — untouched.
- Red-team reports + regression batteries (audit/, battery-v5.md,
  SPREE-*.md) — untouched; historical results treated as history, with
  this receipt recording CURRENT-revision evidence instead.

## Remaining unknowns

1. Live-D1 behavior of `ON CONFLICT DO NOTHING` under true multi-isolate
   concurrency is reasoned-from-SQLite-semantics, not load-tested against
   production traffic (no load test run; $0 constraint).
2. The residual double-read race in the prune path (both see votes<2,
   both stamp, none prune) is fail-safe but untested under real
   concurrency — same constraint as (1).
3. Deployed worker still runs the pre-hardening bundle until someone
   deploys v0.6.16 (deploy standing order exists; deployment was out of
   this campaign's hardening-only scope — no push, no deploy performed).
4. seal_registry growth under adversarial seal-flood: bounded by
   REPLAY_MAX_SEEN memory cap (503 fail-closed) + 500-row/cron-run D1
   prune; sustained flood economics unmeasured.
5. Historical battery reports (v3/v4/v5, redteam2) were not re-run
   against v0.6.16 — the new hostile suite + envelope suite are the
   current-revision evidence; older batteries remain historical.
