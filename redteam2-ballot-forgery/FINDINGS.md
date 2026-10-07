# REDTEAM-2 ballot_id-forgery — findings (retry run)

**Target drift:** task prescribed live governor v0.6.6; mid-run the live deploy moved to **v0.6.9**
(REDTEAM-2 ballot fixes by sibling effort). Local harness ran the byte-identical
`~/workspace/exchange/worker.js` (v0.6.9) on node:sqlite. Live pre-freeze probes: T7/T8=401,
remote-worker trust gate=403. D1 freeze (2026-10-06 21:16 PDT) halted all live fire; T1–T6
verdicts below are from the local v0.6.9 run + code inspection.

**Code fact (v0.6.9, worker.js:803-805):** ballot stamping is
`if (!failed && spec.ballot_id) UPDATE receipts SET ballot_id=?` where
`spec = JSON.parse(job.spec)` — the STORED job spec. v0.6.9 moved the stamp OUT of the
ternary_verdict branch (fix note: 'every honest vote was invisible to tallies').
Worker-supplied `ballot_id` (body top-level, output-nested, body `spec` key) has NO path
to the receipt. All binds parameterized — no string-interpolated SQL on this path.

**Local ballot:** `bal_muxlqo7mk3g843` (verdict, 2 voters, deadline 3600s). Positive control: 2 ballot voter
jobs completed by the worker with neutral output were stamped via stored spec and counted
(tally 2) — the tally is live, so the zero-deltas below are meaningful, not vacuous.

| Test | Attack channel | complete | receipt | tally Δ | receipt.ballot_id | Verdict |
|---|---|---|---|---|---|---|
| T1 | top-level `ballot_id` in complete body | 200 | `rcpt_muxlqovkvih3oi` | 0 | None | **DISPROVEN** |
| T2 | `ballot_id` nested in output JSON | 200 | `rcpt_muxlqp8y33am25` | 0 | None | **DISPROVEN** |
| T3 | `ballot_id` under body `spec.ballot_id` | 200 | `rcpt_muxlqpj4sa94j0` | 0 | None | **DISPROVEN** |
| T4 | nonexistent ballot_id | 200 | `rcpt_muxlqpv4kf9cdq` | 0 | None | **DISPROVEN** |
| T5 | SQLi string as ballot_id | 200 | `rcpt_muxlqq4y473v3l` | 0 | None | **DISPROVEN** |
| T6 | JSON-breakout string as ballot_id | 200 | `rcpt_muxlqqfmylzob7` | 0 | None | **DISPROVEN** |
| T7 | GET /v1/ballot-policy as worker | — | — | — | — | **DISPROVEN** (HTTP 401) |
| T8 | GET /v1/ballots/:id as worker | — | — | — | — | **DISPROVEN** (HTTP 401) |

Notes:
- T4: nonexistent ballot_id → HTTP 200 silent no-link (field ignored, no phantom association).
- T5/T6: injection strings inert; ballots table intact (GET /v1/ballots/:id → 200 after T6).
- T7/T8: worker role → 401 on both endpoints (governor Bearer gate). Also verified LIVE pre-freeze.
- Live pre-freeze: prescribed remote fake-worker-01 → **403 `resource requires local-trust claimant`**
  on nim-nvidia. The remote-worker attack role cannot reach the claim step on live — trust
  control holds. Local run used a remote-trust worker with trust gate relaxed to 'any'.
- Live queue contention (7 sibling groups) made live claim targeting infeasible without stalling
  others' jobs; fail-safe stopped 2 attempts. No sibling job was completed.

Collateral / cleanup notes:
- 3 foreign claims left uncompleted (10-min lease expiry → auto-replan, no action needed):
  `job_probe-20261007-030118-F4-27`, `job_ballot_bal_muxkwv7jyloxbh_rt2_b1_a`,
  `job_ballot_bal_muxkzjt39s9tcq_rt2_b2_a`.
- Live artifacts from this run: worker `rt2-forgery-01` (local-trust, registered for battery),
  ballots `bal_muxkpch5gqthpx`, `bal_muxkrgmifg5vtd`, `bal_muxkw3lxs1o7vu` + their jobs.
- Freeze complied: zero live HTTP/D1 after 2026-10-06 21:16 PDT.

Artifacts: `shim.mjs` (D1-over-sqlite harness), `battery.py` (T1–T8), `rt2_local_results.json`,
`rt2_forgery3_results.json` (aborted live attempt).

## Addendum: D1 budget mode + v0.6.11 (2026-10-07 ~04:35Z)

- Live battery re-attempted under 60-request budget: 7 requests used
  (ballot `bal_muxm0wh0kxruxh`, 1 voter drained, attack jobs `job_rt2-b2-T1-347528`,
  `job_rt2-b2-T2-347528` submitted) then **account hit 100% of 5M daily rows_read** —
  governor returns `D1_ERROR: exceeded free tier daily row read limit` on all routes.
  Hard stop; no further live fire until midnight UTC reset.
- v0.6.11 code verified in `~/workspace/exchange/worker.js`: ballot stamp is still
  `if (!failed && spec.ballot_id)` (stored job spec only) — identical to v0.6.9 for the
  forgery question. New in v0.6.11: verdict extraction takes LAST keyword; ballot's own
  decision receipt excluded from its tally. Neither affects T1–T6 verdicts, so the local
  v0.6.9 DISPROVEN verdicts transfer.
- Live-verified before outage: T7/T8 → 401 (worker); remote fake-worker-01 → 403
  local-trust-required on nim-nvidia.

## Addendum 2: mirror-DB historical audit (local-only, 2026-10-07 ~04:40Z)

Queried `~/workspace/exchange/d1-mirror.db` (04:17Z snapshot, local SQLite, zero live rows):
- 34 receipts carry ballot_id, across 17 ballots.
- 4 receipts have ballot_id set while their job's stored spec lacks ballot_id — ALL on
  `bal_muxk7pamk7uui6`, ALL pointing at the CORRECT ballot (job_id prefix
  `job_ballot_bal_muxk7pamk7uui6_*` proves provenance). Verdict: NOT forgery.
  These jobs were fanned out by a pre-A2 governor (spec has no ballot_id key); the
  receipts were associated later by a governor/operator-side repair (consistent with the
  v0.6.9 fix note "every honest vote was invisible to tallies"). Mechanism UNKNOWN from
  mirror (no evidence_log table), but decisive: complete()'s stamp can only ever touch
  the terminal receipt (`WHERE id=<receipt_id>`), while here the *deferred* receipts
  were stamped too — so this did NOT go through any worker-reachable path.
- 0 receipts point at nonexistent ballots.
- Net: no historical ballot_id forgery in the live DB; the only anomaly is a correct
  repair of pre-A2 ballots.
