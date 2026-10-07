# GEPPETTO HARD — Agent C: Phases 4–6 (State / Accounting / Time)

**Scope:** `~/workspace/exchange/worker.js` (v0.6.0, 2053 lines) + live D1.
**Method:** code-traced every transition, verified against live D1, empirical SQL tests.
**Constitution:** not touched. One implementation bug fixed (evidence-justified).

---

## PHASE 4 — THE REAL STATE MACHINE

### Job: `submitted → running → done | blocked | failed | deferred`
- `submitted→running`: `runJob` entry. `running→done/blocked/failed`: terminal, receipt written.
- `running→deferred`: (a) all attempts failed but ANY taxonomy retryable + defer_count<8; (b) delegated (parked for worker); (c) no candidate, ANY rejection retryable.
- `deferred→running`: cron replan ONLY (bounded 2/run, defer_count<8), after live-delegation check.
- **Multi-meaning state:** `deferred` = {awaiting worker pickup, awaiting replan, awaiting reality change}. Disambiguated at replan time by the leases+claims live check — not by the state value itself. PROVEN in code (cron lines 2063-2072).
- **Unreachable:** `submitted` never persists (submitJob → runJob synchronously). `queued` (in schema comment) never written. DEAD states in the comment, not the machine.

### Claim: `active → completed | expired`
- `active→completed`: `/v1/complete` success path. Idempotent replays return original.
- `active→expired`: `expireClaim` (late completion OR zombie sweep).
- **Terminal:** no outgoing transitions from `completed`/`expired`. PROVEN (grep: only two UPDATE sites, both set terminal states).

### Lease: `active → claimed → consumed | released | expired | invalidated`
- `active→claimed`: atomic conditional UPDATE in `/v1/claim` (W1).
- `claimed→consumed/released`: completion (success/fail). `→expired`: sweep/late-complete.
- `active→invalidated`: inline-path lease revalidation failure. `active→released`: orphan reaper.
- `active→expired`: `validateLease` on access.
- **Double-fireable:** guarded — every transition checks current state first (`WHERE state='active'`, `claim.status` checks). PROVEN.

### Execution: `running → ok | error | uncertain`
- `running→uncertain`: `expireClaim` ONLY (line 750 — single write site, grep-verified).
- **UNCERTAIN is terminal and honest:** no code path transitions FROM `uncertain`. Live D1 shows 2 uncertain executions (W4/W5 battery), both terminal, both with "provider may have executed" in the error field. **PROVEN: UNCERTAIN never silently becomes SUCCESS/FREE/FAILED.**
- **Multi-meaning:** `result='running'` covers both inline-running and claimed-awaiting-worker. Disambiguated by `claim_id` (NULL = inline). Acceptable but implicit.

### Evidence: `UNKNOWN → DISCOVERED → ACCESSIBLE → FREE → QUOTA → AVAILABLE → VERIFIED` (+ `PARKED`)
- Promotion: `autoPromote` (observations+receipts only, never config). Demotion: `demoteCheck` (staleness), `billingCheck`, quarantine.
- `PARKED`: credential-invalid; only explicit re-verify clears it (verify route), never silent probes. PROVEN.
- **Unguarded transition found:** `setEvidence(force=true)` bypasses the rank check — used by quarantine-demote, billing-appeared, stale-demote. All call sites are governor-internal with evidence_log rows. Acceptable, but `force` is a loaded gun with 5 triggers.

### Resource health: `UNKNOWN → AVAILABLE | DEGRADED → THROTTLED | QUARANTINED | EXHAUSTED`
- `observe()` rollup: 3 consecutive failures → QUARANTINED; latest ok → AVAILABLE/DEGRADED by rate.
- 429 → THROTTLED (exponential backoff, overrides rollup). AUTH_ERROR → credential invalid + PARKED.
- **Race:** `observe()` reads last-3 observations then writes state — two concurrent completions could interleave, but D1 single-writer serializes; worst case is a slightly stale rollup, self-corrects on next observation. PLAUSIBLE, not proven harmful.

### FINDINGS (Phase 4)

**F1 [CRITICAL — FIXED + DEPLOYED]: Temporal comparison bug.**
`lease_expiry`/`expires_at` are ISO-8601 (`2026-10-06T22:25:38.777Z`) but `sweepExpiredClaims` and the cron replan live-check compared against SQLite `datetime('now')` (`2026-10-06 22:20:00`). String comparison: `'T'(0x54) > ' '(0x20)` → same-day expiries NEVER evaluated as expired. Proven empirically:
- `'2026-10-06T10:00:00.000Z' < datetime('now')` → 0 (FALSE — wrong)
- `'2026-10-06 10:00:00' < datetime('now')` → 1 (TRUE — correct)
Impact: (a) zombie sweep silently skipped same-day expiries (recovery delayed up to 24h); (b) replan live-check treated expired same-day leases as live (jobs stuck deferred). Fix: bind ISO `new Date().toISOString()` as comparison parameter in both sites. Deployed v0.6.0. Does not alter the protocol — implements the documented behavior.

**F2 [MEDIUM]: 16 orphan `active` leases with `expires_at=NULL`.**
Pre-v0.6 test rows (job_adv*, job_final*, job_failover*, job_smoke1). `validateLease`: `Date.parse(null)` → NaN → `NaN < now` is false → these leases NEVER expire via validation. Harmless today (jobs terminal, replan check requires `expires_at > now` which NULL fails), but they are unaudited authority tokens. Recommend: one-time cleanup or backfill expiry.

**F3 [LOW]: Legacy orphans from v0.5 cutover.**
1 `running` execution (exec_mux7s5rk132ac7_a1, job_nim-proof-001) + 1 `active` lease (lease_mux7s5w8smid4f) — pre-v0.6 claim flow, job is `done`. Nothing references them. Harmless.

**F4 [INFORMATIONAL]: `force=true` in setEvidence has 5 triggers.**
All governor-internal, all logged. Not a bug, but the demotion guard is one boolean away from being bypassed. Worth a comment, not a change.

---

## PHASE 5 — ACCOUNTING AS EPISTEMICS

Live: 11 rows, all `measured_cost=0`, all `cost_basis='measured'` (10 fake-deterministic battery + 1 nim-nvidia).

| Field | Source | Authority | Method | Epistemic status |
|---|---|---|---|---|
| `measured_cost` | `costOf(resource)` in `completeClaimed` | governor | `cost_class=='free'` → 0, else `config.usd_per_unit` | MEASURED (of the resource's declared economics, not of actual spend) |
| `cost_basis` | `resource.cost_basis` | governor | set to `'measured'` on any $0 completion | MEASURED (but coarse — see F6) |
| `quota_effect` | `quotas.used_window` before/after | governor | `quotaConsume` counter | MEASURED if quota row exists; else **MISSING disguised as {0,0}** (see F5) |
| `recorded_at` | `datetime('now')` DEFAULT | D1 clock | insertion time | OBSERVED_AT |
| `execution_id`/`claim_id`/`job_id` | claim chain | governor | foreign keys | PROVENANCE (full chain traversable) |

**Worker assertions:** explicitly ignored — `b.cost` is never read; receipt notes "worker_cost_assertion: ignored — governor's books". W6 battery proved `$99999` → books show `$0`. PROVEN.

**The 2 missing v0.6 rows:** honest gap, deliberately not backfilled. CORRECT per doctrine. Receipts survive with `cost=0`. The absence is itself evidence of the deployment-window fault.

**F5 [MEDIUM]: `quota_effect={quota_before:0, quota_after:0}` for quotum-less resources is MISSING, not zero.**
`quotaConsume` is a no-op without a quota row, so before/after are both 0 — indistinguishable from "measured zero consumption". A reader cannot tell "no quota tracked" from "quota tracked, nothing used". Recommend: `quota_effect=null` when no quota row exists.

**F6 [LOW]: `cost_basis='measured'` is coarse.**
Set on ANY $0 completion, including the fake worker's. It means "observed a $0 execution", not "audited the provider's billing". For NIM this is fine (no billing relationship exists), but the label overclaims for paid-capable resources. The `accounting.cost_basis` column has the same granularity. Recommend: distinguish `measured-zero` (observed $0) from `measured-billed` (provider invoice reconciled) when paid resources arrive.

**F7 [INFORMATIONAL]: Receipt `cost` duplicates `accounting.measured_cost`.**
Same value, two places. Not contradictory, but the system of record for cost should be exactly one table. Currently `accounting` is authoritative (governor's books); receipt `cost` is a denormalized copy. Acceptable.

---

## PHASE 6 — TEMPORAL EPISTEMIC AUDIT

### Timestamp formats (3, now 2 in comparisons)
| Producer | Format | Example | Used for |
|---|---|---|---|
| `now()` (JS) | ISO `T` + ms + Z | `2026-10-06T22:15:38.777Z` | leases.expires_at, claims.lease_expiry, executions.started_at |
| `datetime('now')` (D1) | space, sec precision | `2026-10-06 22:15:38` | created_at/observed_at/recorded_at DEFAULTs |
| `minutesAgo(n)` (JS) | space, sec precision | `2026-10-06 22:15:38` | observation window queries |

`ts()` parses both (replaces space with T). JS `Date.parse` handles ISO. The ONLY broken comparisons were the two fixed in F1.

### Temporal labels (as implemented)
| Field | Label | Notes |
|---|---|---|
| `health_observations.observed_at` | OBSERVED_AT | when the governor recorded the observation |
| `evidence_log.at` | DECIDED_AT / KNOWN_AT | when the transition was recorded; `detail` cites evidence |
| `receipts.created_at` | DECIDED_AT | receipt write time |
| `receipts.started_at` / `finished_at` | OCCURRED_AT | execution window |
| `claims.claimed_at` | OCCURRED_AT | pickup event |
| `claims.lease_expiry` | VALID_AT (until) | authority validity window |
| `leases.created_at` | DECIDED_AT | reservation event |
| `policy_version` (receipts, claims.policy_snapshot) | DECIDED_AT binding | sha256 of policy set at decision time — **this is the good pattern**: preserves what policy was in effect when the decision was made |
| `jobs.spec` | DECIDED_AT (immutable) | never UPDATEd; verifier recomputes input_hash from it |

### Future-information leakage: NOT FOUND (in the places tested)
- **Receipt verifier** recomputes `input_hash` from the CURRENT `jobs.spec` — safe ONLY because spec is immutable (no UPDATE path exists; grep-verified). If spec were ever mutated, old receipts would fail verification — fail-closed, correct direction.
- **Delegation receipt** carries the 10-gate trace from delegation time; the completion receipt references it rather than re-deriving. What was known at decision time is preserved, not overwritten. CORRECT.
- **`policy_snapshot`** in claims freezes max_cost/data_class/spend_authority at claim time. A later policy change cannot retroactively alter the claim's authority. CORRECT.
- **`warrantedTier`** evaluates CURRENT observations at decision time — this is legitimate (promotion IS a present-tense judgment), not leakage.
- **F8 [LOW]: `evidence_at` records promotion time but the promotion doesn't cite WHICH observations warranted it.** `autoPromote` logs "warranted by recorded observations" without observation IDs or timestamps. A future auditor cannot reconstruct exactly what was known at promotion time — only that *something* warranted it. Recommend: include the warranting observation/execution IDs in the evidence_log detail.

### Clock skew
Edge (`Date.now()`) vs D1 (`datetime('now')`) clocks can disagree by seconds. Lease creation and expiry now both use edge ISO (after F1 fix) — consistent. Observation windows use D1 clock consistently. The 60s lease / 10-min pickup windows dwarf skew. NOT a practical issue.

---

## Summary of repairs (this agent)

1. **F1 temporal fix** — bound ISO timestamps in `sweepExpiredClaims` and cron replan live-check. Deployed v0.6.0. No protocol change.
2. **F5/F6/F8** — documented as recommendations, NOT implemented (need Zackary's call on label semantics; they touch the frozen accounting vocabulary).

## Verdicts

- **UNCERTAIN**: PROVEN terminal and honest (single write site, no outgoing transitions, 2 live rows terminal).
- **Idempotency**: PROVEN for completion (W2); claim atomicity PROVEN (W1).
- **Accounting epistemics**: STRONGLY SUPPORTED for cost (governor-sourced, worker-ignored); WEAK for quota on quotum-less resources (F5).
- **Temporal integrity**: PROVEN sound after F1 fix, except F8 (promotion provenance).
- **State machine**: no impossible states reachable in v0.6 paths; legacy orphans (F2/F3) are pre-v0.6 debris, not live bugs.
