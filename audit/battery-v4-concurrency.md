# Battery v4 — Concurrency Group (re-run)

**Target:** live Exchange governor **v0.6.3** (`lucidota-exchange.cutecity.workers.dev`, version confirmed `0.6.3` at test start)
**Date:** 2026-10-07 ~03:38–03:50 UTC (Oct 6 ~20:38–20:50 PDT)
**Script:** [battery-v4-concurrency.py](sandbox://workspace/exchange/audit/battery-v4-concurrency.py)
**Raw results:** `/tmp/battery-v4-conc-full.json` (R2+R1+R3), `/tmp/battery-v4-conc-r1r2.json`, log `/tmp/battery-v4-conc.log`

## Method note (deviation from the brief)

The brief asked for a strict single-job claim race (drain queue → submit 1 job → race).
At test time the deferred queue held **50+ live jobs from other lanes** (lane-b-integration,
lane-e-redteam, lane-g, integrity-battery-v4 — all actively producing). Draining by
claim+complete would have meant completing other lanes' jobs — corrupting their experiments
with dummy output, or stranding them. Instead:

- **R2** ran as 12 barrier-synchronized 2-worker race rounds on the live queue, comparing
  `job_id`/`lease_id` in **every** delegation. This directly answers the open question
  ("did the 2 winners claim the SAME job?").
- Every won claim was completed **honestly via real NIM inference** (same code path as
  `nim-claimant.py`, vault-held key, tiny inputs) — no dummy output was ever written
  to another lane's job. $0 total.

## R2 — claim race: PASS

12 barrier-synced rounds, W1 (`~/.exchange_worker_nim`) vs W2 (`~/.exchange_worker_nim_02`).

| round | winners | w1 lease | w2 lease | note |
|---|---|---|---|---|
| 0,1,3,4,5,6 | 2 | ≠ | ≠ | different leases — requests serialized, correct |
| 2 | 1 | `…2px4l7yj` | — | w2: **"lost race or lease not active"** — true contention, atomic win decided exactly one winner |
| 7–11 | 0 | — | — | queue drained, both `claimed=false` |

- **Same-lease double-grant: 0/12.** No `lease_id` was ever granted to both workers in one round.
- **Same-job double-win: 0.** Every 2-winner round had different `job_id`s.
- **No `claim_id` was ever reused.**
- **Verdict: PASS.** The atomic win (`UPDATE leases … WHERE state='active'`, `meta.changes` as arbiter) holds under contention.

**Resolution of the initial probe's open question:** the v0.6.2 probe's "2 winners" is
**explained, not a bug** — the queue held 2+ jobs and the two claims serialized onto
different leases (the same regime as rounds 0,1,3,4,5,6 here). The probe never compared
`job_id`s; this re-run did, every round.

## R1 — double-complete: FAIL (HIGH)

One real claim won by W1 (`claim_muxka6djeaemwp` on `job_probe-20261007-033953-F10-3`),
exactly one NIM execution, then **two barrier-synchronized `/v1/complete`** with
identical bodies.

| | r1 (finished 1st) | r2 (finished 2nd) |
|---|---|---|
| HTTP | 200 | 200 |
| `state` | done | done |
| `idempotent` | **false** | **false** |
| `receipt_id` | `rcpt_muxkabzktti1ah` | `rcpt_muxkabtne6f4my` |

Post-check via `GET /v1/job/:id`:
- **2 receipts** for the claim (`rcpt_muxkabzktti1ah`, `rcpt_muxkabtne6f4my`, both `result=ok`)
- **2 accounting rows** (`acct_muxkabg1tkh9ir`, `acct_muxkabm420vf8u`, both `measured_cost=0`)
- 1 execution row (created at claim time — correct), claim status `completed`

**Verdict: FAIL — true double execution, not a flag artifact.** Both completes ran the
full `completeClaimed` path. Neither hit the replay branch (which is the only path that
sets `idempotent: true`).

**Root cause:** the idempotency guard is a non-atomic check-then-act. `completeClaimed`
does `SELECT … FROM claims WHERE claim_id=?`, branches on `status === "completed"`, then
executes ~15 statements (observe, quota, accounting INSERT, execution UPDATE, lease UPDATE,
job UPDATE, receipt INSERT, …) before finally `UPDATE claims SET status='completed'`.
Two overlapping requests both read `status='active'`; D1 serializes the statements but
nothing makes the check-and-set atomic.

**Impact:**
- Breaks the Worker Protocol v1 invariant W2 ("replays return the original result with
  **no duplicate accounting**") and the code comment at `completeClaimed` ("No new receipt,
  no new execution, no double accounting").
- Any worker retry after a timeout (the normal failure mode!) double-accounts. On a paid
  resource this double-charges; on a quota'd resource it double-consumes quota.
- Receipt uniqueness violated: 2 receipts, 1 execution — downstream receipt-chain/audit
  consumers see a phantom execution.
- This also reframes the original probe observation: `idempotent=false` on the second
  response was **not** a check-order artifact — it was both requests genuinely executing.

**Fix recommendation:** make completion-claim atomic the same way the claim path is —
conditional status flip FIRST:
`UPDATE claims SET status='completing' WHERE claim_id=? AND status='active'`, branch on
`meta.changes`. Loser reads the winner's `completion_summary` and returns the replay
(`idempotent: true`). Must handle crash-recovery for a stuck `completing` state
(zombie sweep currently only covers `active`+expired lease).

*Note: the double receipt/accounting rows on `job_probe-20261007-033953-F10-3` are
deliberately left in place as evidence — do not delete (append-only rule).*

## R3 — queue depth hold (35–38)

8 submit threads, `max_cost=0`, keys `b4-conc-N`, 429 → 60 s backoff per thread, 8 min.

- **Submitted 50, accepted 50, 429s 0, errors 0.**
- Depth trace (deferred count, LIMIT 50): `24 → 36 → 43 → 47 → 50(capped)` then pinned
  at cap. In-band [35,38]: 7/34 samples.
- The band controller worked (paused my submits at ≥38, resumed <35), but **external
  producers (lane B/E batteries) kept pushing the queue above the band** — depth is a
  shared resource, one battery group cannot hold it alone.
- **0 × 429 across 50 submits** while deferred sat at the 50 cap: confirms deferred count
  (includes expired-lease zombies awaiting cron replan) ≠ `unclaimedQueueDepth` (the live
  backpressure metric, per-resource active leases). The 429 gate never tripped because
  live unclaimed depth stayed < 40.

## Follow-ups

1. **R1 double-complete is a HIGH bug — fix before any lane trusts `idempotent` or
   receipt-counts.** Suggested owner: governor hardening lane. Regression test: this
   battery's R1 (barrier double-complete + receipt/accounting count).
2. R2 needs no fix. Consider adding the per-round `job_id` comparison to the standing
   battery so a future "2 winners" never ambiguates again.
3. R3's band-hold needs cross-lane coordination (or a dedicated quiet window) to be
   meaningful — as run, it measured shared-queue dynamics, not governor response.
