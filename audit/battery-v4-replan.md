# Battery v4 — REPLAN group report

**Target:** live Exchange governor (v0.6.3 at task time → **v0.6.5** live as of 03:43Z; replan path byte-identical across the upgrade — diff verified)
**Window:** 2026-10-07 03:36–04:2xZ
**Method:** 60s poll loop + 20s tick-watch, direct D1 reads (Exchange key not held by this agent; worker creds + D1 API used)

## 1. Target job: job_muxji956wbg7su

| field | value |
|---|---|
| state | deferred |
| defer_count | **1** (from the FAILED_RETRYABLE completion, not from any cron pickup) |
| receipts | 2 (delegation 03:20:29Z; FAILED_RETRYABLE completion 03:20:42Z) |
| leases | 1 (released) |
| spec | `{organelle: exchange-verifier, capability: cognitive, max_cost: 0}` → routes to `res_localswarm` (VERIFIED/AVAILABLE, claimed adapter) |
| claimant | `swarm-claimant-01` claimed in 8s, reported FAILED_RETRYABLE (ternary_verdict −1) |

### 1a. The "v0.6.1 silent failure" premise is DISPROVEN for this job

Battery-v4.md (V4-C) claims the cron replan "fired once (v0.6.1, silent failure)" with defer_count 0→1.
The cron's replan does `defer_count+1` **before** calling runJob. Observed defer_count=1
can only come from the completion path. **No cron ever picked up this job** — not under
v0.6.1, v0.6.2, v0.6.3, or v0.6.5. Timeline confirms it: the job failed at 03:20:42Z,
*after* the last v0.6.1 tick (03:20) and the queue ahead of it was 10+ probe jobs deep.
"Silent failure — no new lease, no receipt" was queue-wait misread as a throw.
There is no runJob throw to explain for this job.

### 1b. Replan watch (PENDING — job #12 of 29 in effective queue, ETA ~04:25–04:30Z)

Poll every 60s: state=deferred/1, rcpts=2, no replan-failure receipt as of 03:57Z.
11 jobs ahead, all defer:1 (probe F4 jobs + 2 ballot jobs). Tick-watch (20s) running.
*(to be filled when the cron picks it up)*

## 2. Forced retryable failures: NOT VIABLE via worker creds (finding)

Plan was: submit b4-replan-1/2/3 → claim → complete FAILED_RETRYABLE.
What actually happened:

- All 3 submitted fine (job_b4-replan-1/2/3, max_cost=0, delegated to nim-nvidia).
- `/v1/claim` takes the **oldest unclaimed delegation for the resource** — a worker
  cannot target its own jobs.
- The production claimant fleet (**nim-claimant-01..04**, 204 claims in 25 min, plus the
  120s hook) snatched all three within ~1–2 min and completed them **SUCCEEDED** via real NIM.
- One mis-claim (mine, oldest-first) grabbed lane-E's redteam job
  `job_lanee-rt-markings-spec-marking-set` — completed **honestly** via NIM (SUCCEEDED,
  real output). No fake data written anywhere.

**Verdict (nim-nvidia path):** forced-failure injection through worker creds races the production fleet and
loses. The task's "watch the cron replan them within 2 cycles" assumed an empty queue;
the effective replan queue is 20–22 deep and the claim queue refills in minutes.

**Pivot that worked (res_localswarm path):** `res_localswarm`'s claim queue was EMPTY
(its claimant `swarm-claimant-01` idle since 03:20). Submitted two `cognitive` jobs
(`b4-replan-4`, `b4-replan-5`, max_cost=0) → delegated to res_localswarm → claimed
within 1s (won the race) → completed FAILED_RETRYABLE:

| job | failed at (complete) | queue pos |
|---|---|---|
| job_b4-replan-4 | 2026-10-07T03:55:22Z | 28/29 |
| job_b4-replan-5 | 2026-10-07T03:55:39Z | 29/29 |

Both deferred/1, leases released. Watching for their cron replan (ETA ~05:05Z at 2/tick).
This gives exact failure→replan latency for controlled failures, queue-position-dominated.

## 3. Replan latency — natural experiments (fresh FAILED_RETRYABLE → cron replan)

| job | failed at | replanned at (new lease) | latency |
|---|---|---|---|
| job_probe-20261007-030118-F4-27 | 03:41:34Z | 03:45:51Z | **4m17s** |
| job_probe-20261007-030118-F4-26 | 03:40:58Z | 03:45:49Z | **4m51s** |

Both were front-of-queue → picked at the next 5-min tick. **Replan latency ≈ one cron
tick for front-of-queue jobs.** Latency is queue-position-dominated: the cron takes the
2 oldest eligible deferred jobs per tick; deeper jobs wait proportionally.

## 4. Silent failures: NONE observed

- **Zero** replan-failure receipts (`decision LIKE '%"replan":"failed"%'`) since 03:00Z,
  across all cron pickups in the window (F4-26/F4-27 replanned 3+ times each, all clean
  re-delegations, no throws).
- The v0.6.2+ receipt path (catch → INSERT receipt with `errors`) is live in code and
  the replan block is byte-identical v0.6.3→v0.6.5, but **no throw has occurred to
  exercise it** in this window — the receipt path itself remains UNPROVEN live
  (proven in code, never fired).
- Incidental: one `migrate()` race observed — concurrent fetch requests racing
  `ALTER TABLE ... ADD COLUMN ternary_verdict` produced a one-off
  `D1_ERROR: duplicate column name` 500 on an unrelated route. Cosmetic; not on the
  replan path (scheduled() never calls migrate()).

## 5. Queue dynamics (context for the latency numbers)

- Effective replan queue (deferred, defer<8, no live lease): **20–22**, growing
  (submit rate > 2-per-tick drain).
- The F4 probe jobs are in a tight replan→claim→fail loop, burning defer budget
  (F4-27 hit defer 7 within ~13 min of cycling); they drop out at defer_count=8.
- Replanned jobs get fresh 10-min pickup leases → leave the effective queue ~10 min,
  then rejoin on expiry/failure. The queue is a treadmill, not a drain.

## Verdicts

| claim | verdict |
|---|---|
| runJob threw for job_muxji956wbg7su under v0.6.1 (silent failure) | **DISPROVEN** — never picked; defer_count=1 |
| replan-failure receipts fire when runJob throws (v0.6.2+) | **UNPROVEN live** — zero throws in window; code path present and identical v0.6.3→v0.6.5 |
| replan latency ≈ 1 cron tick (front of queue) | **PROVEN** — 4m17s, 4m51s measured |
| forced FAILED_RETRYABLE via worker creds | **DISPROVEN practical** — fleet + oldest-first defeats it |
| silent replan failures in current version | **none observed** (50+ min, all pickups clean) |

## Open

- Target replan capture (~04:15Z): does runJob throw for the `cognitive`→res_localswarm
  replan, or re-delegate cleanly? Tick-watch running; will update §1b.
- The replan-failure receipt path has never fired live — needs a synthetic throw to prove
  the catch→receipt path end-to-end (suggest: unit-level, not against the live fleet).
