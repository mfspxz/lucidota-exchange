# Worker Protocol v1 — FROZEN

**Status:** frozen 2026-10-06. Changes require Zackary's explicit approval.

## Amendments (approved 2026-10-07, live in governor v0.6.3)

- **A1 — Governor-side ternary extraction.** A worker's completion output may
  carry `ternary_verdict` (+1 proven | 0 unknown | -1 disproven). The governor
  extracts it into a first-class `receipts.ternary_verdict` column — never
  buried in JSON. The model is the subject, never the judge.
- **A2 — Auto-prune on -1.** When a -1 verdict lands, the governor marks
  dependent deferred jobs (same `input_hash`, or `spec.depends_on` pointing
  at the disproven job) as failed with taxonomy `DISPROVEN_DEPENDENCY`, one
  prune receipt each. O(1) prune, no replan, no consensus debate.
- **A3 — Governor-minted `needs_evidence`.** A 0 (unknown) verdict on a
  succeeded execution mints a `needs_evidence` job the swarm claims via
  `/v1/claim`. The Exchange is the queue — no daemon. Bounded
  (`evidence_depth < 2`, evidence jobs never mint further evidence).

## Voting amendments (approved 2026-10-07, live in governor v0.6.4)

Voting is a governor composition pattern, not a worker behavior. Workers
never know they are voting; each vote is one ordinary delegated job and
the tally is a deterministic reduction over N receipts. All 10 worker
invariants hold by construction.

- **A1 — `/v1/ballots` endpoints.** `POST /v1/ballots`
  (`ballot_type` verdict|answer, `proposition`, optional `voters`,
  `deadline_s`) fans out one job per voter — all-or-nothing under
  backpressure (429 if N voters don't fit under the queue cap).
  `GET /v1/ballots/:id` returns the deterministic tally. New objects
  only; no existing endpoint changes shape.
- **A2 — receipt `ballot_id`.** Optional, additive column linking N vote
  receipts to one ballot. Existing receipts unaffected.
- **A3 — quorum policy object.** `GET /v1/ballot-policy`: governor-side
  config (default voters, quorum rule, deadline, tie policy). No worker
  visibility.

Tally rules: strict >50% of valid votes wins; tie/no-majority →
INCONCLUSIVE → UNKNOWN (routes to the evidence workflow, never
fail-open); below quorum at deadline → EXPIRED. Dissent is kept on the
ballot receipt in full. Vote extraction is mechanical (regex / normalized
exact-match) — no LLM judging the tally.

### Voting hardening (REDTEAM-2, 2026-10-07)

- `deadline_s` validated: 400 on non-finite/non-positive, 30-day cap
  (was: non-numeric → unhandled 500 RangeError). v0.6.7.
- Ballot receipt INSERT carries `job_id` sentinel `ballot:<id>`
  (was: NOT NULL violation → HTTP 500 on every terminal transition,
  audit receipt silently lost). v0.6.8.
- Voters validated + slug-deduped pre-insert: non-string/empty/duplicate
  voters → 400 (was: 500 + orphan jobs executing real inference). v0.6.8.
- `ballot_id` stamped on **every** successful vote-job receipt, not just
  ones carrying `ternary_verdict` (was: all honest worker votes invisible
  to tallies). v0.6.10.
- Ballot decision receipt excluded from its own tally (was: phantom
  INVALID vote on every later read). v0.6.11.
- Verdict extraction takes the LAST keyword in the tail (was: first match
  won — misread the "Final verdict:" line). v0.6.11.
- Tally dedupes to latest receipt per job (jobs write deferred + terminal
  receipts). v0.6.6.

### INV7 enforcement (REDTEAM-2, 2026-10-07)

SUCCEEDED with empty output `{}` → VERIFICATION_FAILURE for
non-inference capabilities too (was: `{}` verified as ok — battery-v2 X11
gap persisted). v0.6.10.
**Exchange version:** 0.6.0.

## Doctrine

The Governor owns authority. The worker owns execution. Neither impersonates the other.

```
EXCHANGE
    │
┌───▼───────┐
│ GOVERNOR  │  gates · policy · leases · quota · accounting · receipts
└───┬───────┘
    │ opaque delegated job
┌───▼───────┐
│ CLAIMANT  │  pickup · validate · execute · capture · report
└───┬───────┘
    │
┌───▼───────┐
│ CAPACITY  │  NIM / OpenRouter / DeepSeek / fake / phone / laptop / …
└───────────┘
```

Execution capacity is replaceable without changing governance. NVIDIA NIM is
one machine at the end of the chain — not the chain.

## Worker invariants

A worker must never be able to:

1. mint a lease — only the governor creates leases (`createLease`)
2. bypass a gate — claim requires a governor-issued deferred delegation
3. choose its own accounting result — cost comes from resource economics, never the worker body
4. mark arbitrary work complete — completion requires a valid claim bound to that worker
5. alter resource authority — workers cannot touch evidence/health/credential state
6. access another worker's secret — workers hold only their own scoped credential
7. convert a failed execution into success — SUCCEEDED with empty output → VERIFICATION_FAILURE
8. manufacture a receipt — receipts are written only by governor code paths
9. extend its own lease — no extend endpoint; expiry is enforced on completion
10. change the requested policy — `policy_snapshot` is read-only and recorded

The worker receives a delegation, not permission to become the Exchange.

## The delegation object

`POST /v1/claim` (worker auth) returns:

```
delegation:
  claim_id             — the claim; all completion binds to this
  resource_id
  capability
  input                — the work
  policy_snapshot      — { policy_version, max_cost, data_class, spend_authority }
  lease_expiry         — ISO timestamp; completion after this is rejected
  attempt              — execution attempt id
  idempotency_key
  receipt_requirements — ["output_hash", "model_identity", "latency_ms", "worker_outcome"]
  credential_ref       — opaque (e.g. "vault:custom.nvidia-nim"); the secret NEVER travels
  requested_model      — what the Exchange asked for (may be null)
```

## Terminal outcomes

The worker reports exactly one:

- `SUCCEEDED` — executed; output captured
- `FAILED_RETRYABLE` — time-bound failure (429, 5xx, timeout, network, empty 200)
- `FAILED_PERMANENT` — will not succeed on retry (401/403, 404/410, 400)
- `QUARANTINED` — worker signals the capacity itself looks bad (evidence; the governor decides)
- `EXPIRED` — worker ran out of time

No "probably worked". No "try again maybe". Those are state-machine transitions.

The worker is boring: PICKUP → VALIDATE → EXECUTE → CAPTURE → REPORT.
No retries. No failover ("NIM failed, I'll try OpenRouter" is governor territory).
One attempt; the governor replans.

## Temporal chain

JOB → CLAIM → EXECUTION → RECEIPT → ACCOUNTING. Distinct objects:

- **JOB** — intent + governed decision (10 gates, lease reservation)
- **CLAIM** — worker bound to a delegation (claim_id, worker_id, lease, policy snapshot)
- **EXECUTION** — what actually ran (requested_model vs actual_model, outcome)
- **RECEIPT** — evidence the Exchange is willing to believe (verification, trace)
- **ACCOUNTING** — the governor's books (measured_cost, cost_basis — never worker-asserted)

Worker output is evidence. Receipt verification establishes what the Exchange
is willing to believe. No execution result is trusted merely because a worker
says it happened.

## Authentication

- Workers: `POST /v1/workers/register` (governor admin, Exchange key) mints
  `worker_id` + `worker_secret` (shown once; only sha256 stored).
- Worker calls use `X-Worker-Id` + `X-Worker-Secret` — never the Exchange master key.
- Resources declare `claimant_trust_required`: `local` (credential-holding,
  e.g. nim-nvidia) or `any` (untrusted remote workers OK).

## WORK AVAILABLE — the seam

The semantic contract is "work available", not "poll every 2 minutes".
Implementations:

- **poll** — current: hook runs the worker every 120s (cost-conscious)
- future: unix socket · local event · HTTP callback · queue · systemd
  activation · WebSocket · mobile push

The worker never knows or cares which implementation delivered the signal.

## v1 acceptance bar

- [x] atomic claim (W1) — one conditional UPDATE decides the winner
- [x] lease expiry (W4) — completion after expiry → LEASE_EXPIRED, job replanned
- [x] completion idempotency (W2) — replays return the original, no double accounting
- [x] execution idempotency semantics — uncertain executions marked UNCERTAIN (W10)
- [x] forged-result rejection (W3) — wrong worker/resource/claim → 403/400/404
- [x] credential isolation — secrets never leave the vault; workers get credential_ref
- [x] secret non-observability (W7) — automated scan, zero leaks
- [x] malformed-provider handling (W8) — 200 {} → FAILED_RETRYABLE, never SUCCEEDED
- [x] crash recovery (W5) — zombie sweep: EXPIRED → deferred → replan
- [x] retry classification — 5 terminals → governor taxonomy
- [x] quarantine — worker signal → governor decision
- [x] receipt binding — claim_id + worker_outcome on every delegated receipt
- [x] accounting binding — separate table, governor's books only
- [x] requested-vs-actual capacity identity (W9) — model_match on execution + receipt
- [x] zero governor changes for new claimant — FAKE_CLAIMANT proved it
- [x] zero Cute-City assumptions — adapters/gates/leases are generic
- [x] deterministic replay test — fake worker, fixed outputs
- [x] adversarial concurrency test — W1 thread race

Protocol frozen. New capacity = new worker implementing this contract.
No new adapters casually.
