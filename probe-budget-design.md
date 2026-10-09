# Probe-Budget Governor — Design

**Status:** DESIGN ONLY. No implementation. Written 2026-10-07 ~09:50 PDT.
**Problem:** On 2026-10-06, REDTEAM-2's live adversarial batteries consumed 100% of the D1 free-tier daily quota (5M rows_read), taking the Exchange itself down at ~21:33 PDT. Verification had no governor. The verifier DoS'd its own control plane.
**Principle:** Verification is a resource-consuming workload. It gets a budget, a cost estimate, execution gating, receipts, and quota accounting — the same discipline the governor applies to everything else.

## 1. Budget model

### 1.1 Classes
Three probe classes, budgeted separately so one cannot starve the others:

| Class | Covers | Default daily share of D1 reads |
|---|---|---|
| `production` | User jobs, claims, ballots, tallies | 60% (3.0M) — reserve floor, see 1.3 |
| `verification` | Redteam batteries, probe harnesses, integrity probes, manual `/v1/probe` runs | 25% (1.25M) |
| `cron` | Scheduled replan loop, staleness demotion, expiry sweeps | 15% (0.75M) |

Writes: D1 free tier allows 100K rows_written/day (documented figure — verify against current Cloudflare limits before implementing). Allocate writes 50/30/20 across the same classes.

### 1.2 Window
One window per UTC calendar day, matching D1's reset (midnight UTC). All budget state keys off `window_start` = UTC midnight. Lazy reset: the first probe touching a new window initializes it; the cron also resets explicitly at its midnight-UTC tick as backup.

### 1.3 Production reserve floor
Probes may NEVER consume the production reserve. Concretely: the `verification` and `cron` classes have hard caps (their shares); they cannot borrow from `production`. Production has no cap other than the account limit — but production jobs already carry `max_cost`/`spend_authority`, which is the existing control. If D1 itself is near the account limit, everything degrades together (fail-closed, as today).

### 1.4 State table (new): `probe_budgets`
One row per (window_start, class): `window_start TEXT, class TEXT, rows_read_limit INT, rows_read_used INT, rows_write_limit INT, rows_write_used INT, state TEXT` where state ∈ `ok | warning | exhausted`. Primary key (window_start, class). State transitions append to `evidence_log` (trigger `probe-budget-transition`).

## 2. Cost estimation (pre-flight)

### 2.1 Battery manifests
Every probe battery must declare a manifest before running:
- `battery_id` (e.g. `redteam2-ballot-v6`)
- `probe_class` (almost always `verification`)
- `estimated_rows_read`, `estimated_rows_write` — per-probe estimates × probe count, plus a 20% contingency headroom
- `max_probes` — hard stop on probe count

No manifest → the governor treats the run as `verification` class with a conservative default estimate and a small `max_probes`. Undeclared large batteries are the thing that killed us; the default must be small enough to be safe.

### 2.2 Calibration loop
Estimates start as operator-supplied guesses. After each run, the receipt records `estimated` vs `actual`; a rolling ratio per battery_id auto-tunes future estimates (exponential moving average, documented in the receipt). Estimates are allowed to be wrong; they are not allowed to be absent.

### 2.3 Pre-flight check
Before a battery starts, the governor:
1. Reads the current window's `verification` budget row.
2. Compares `estimated_total` (+ headroom) against `limit - used`.
3. Returns one of: `allow` | `allow_partial(max_probes=N)` | `defer_until_next_window` | `block`.

The check itself is one indexed read — negligible cost. The decision is written to `evidence_log`.

## 3. Execution gating (new governor gate: PROBE_BUDGET)

### 3.1 Classification
A job/claim is a probe if any of: it carries `is_probe: true` in its spec; its capability is in the verification set (`integrity-probe`, `battery-*`); or it was submitted via a probe endpoint. The INTEGRITY gate's own revalidation probes and the cron's probing reads are classified `verification`/`cron` respectively — the governor's own overhead must be budgeted too, or the design has a hole where the last failure lived.

### 3.2 Gate behavior
- `ok` → execute normally.
- `warning` (≥80% of class budget used) → execute, but the receipt carries `budget_warning: true` and the operator is notified via the existing alerts path.
- `exhausted` → do NOT execute. Return 429 with `reason: "probe_budget_exhausted"`, `retry_after_s` until next UTC midnight, and write a `blocked` receipt (cheap: one write, no probe work). This is the exact failure mode inverted: instead of the probes silently eating production's quota and 500ing everything, the probes fail fast and cheap while production keeps its reserve.

### 3.3 Fail-closed
If the budget row cannot be read (D1 error, corrupt state), probes DEFER — they do not run blind. Production jobs are unaffected by this rule (they have their own economics); only probe-class work is gated.

## 4. Actual-cost accounting (receipts)

### 4.1 Measurement source
D1 query responses carry `meta.rows_read` / `meta.rows_written` per statement. The probe execution path wraps its D1 calls with an accumulator keyed to the running probe (battery_id + probe index). On completion — ok, failed, or blocked — the totals are final.

### 4.2 Receipt fields (added to the existing receipts table via `ensure()`)
- `rows_read_estimated`, `rows_write_estimated`
- `rows_read_actual`, `rows_write_actual`
- `budget_class`, `battery_id`
- `calibration_delta` (actual/estimated ratio, for the tuning loop in 2.2)

### 4.3 Attribution rule
Reads/writes inside a probe execution context attribute to that probe. Ambient reads outside any probe context (migrations, health checks, untagged requests) attribute to a shared `overhead` bucket counted against the `cron` class — visible, not hidden. If overhead ever exceeds 5% of the daily budget, that's itself evidence_log-worthy (it means something is doing untracked work).

## 5. Quota accounting and transitions

- After each probe receipt, `rows_read_used` / `rows_write_used` increment for the class+window.
- Thresholds: `warning` at 80%, `exhausted` at 100%. Transitions append to `evidence_log`.
- Midnight UTC: new window rows initialized (lazy or cron). Old windows retained for audit (they're tiny).
- Operator override: `POST /v1/probe-budget` (master-key gated, like the other admin routes) can raise/lower a class limit mid-window; every override writes a receipt + evidence_log entry with the old and new values and a reason string. No silent changes.

## 6. Admin visibility

- `GET /v1/probe-budget` → current window: per-class limit/used/remaining/state, plus top-5 batteries by actual consumption this window. Cheap (indexed reads + one small aggregation).
- The existing `/v1/receipts` pagination (exch-011) already lets an operator audit per-probe costs; the new receipt fields make that query meaningful.

## 7. Integration points (what touches what)

| Component | Change |
|---|---|
| Governor `handle()` | New PROBE_BUDGET gate evaluation for probe-class work, before execution |
| `POST /v1/ballots`, battery runners, `probe-harness.py`, `worker-battery.py` | Must submit manifests (battery_id, estimates, max_probes) |
| Cron `scheduled()` | Its own D1 reads attribute to `cron` class; replan receipts (exch-025) already give us the paper trail |
| INTEGRITY gate | Its revalidation probes classify as `verification`, not free |
| `migrate()` | Runs under `overhead`; the isolate-level guard (seq 855) stays — the budget is defense in depth, not a replacement |
| Alerts | `warning`/`exhausted` transitions reuse the existing alert path |

## 8. Non-goals (explicitly out of scope)

- No changes to production job economics (`max_cost`, `spend_authority`, backpressure thresholds).
- No provider-side changes; this is purely governor-side accounting.
- No cross-day borrowing: unused budget does not roll over. (Rollover creates perverse incentives to hoard probe capacity.)
- No per-operator quotas (single-operator system; the class model suffices).

## 9. Acceptance criteria (for the future implementation)

1. Replaying the 2026-10-06 REDTEAM-2 battery manifest against this design blocks at the `verification` cap with 429 `probe_budget_exhausted` receipts — and the Exchange stays up.
2. A battery with no manifest gets the small conservative default, not an unbounded run.
3. Estimated-vs-actual calibration converges: after 3 runs of the same battery, estimates within ±30% of actual.
4. Budget override writes a receipt; no silent changes possible.
5. D1 failure during a budget read defers probes without affecting production jobs.

## 10. Open questions (for Zack, not blockers)

1. Are the default class shares (60/25/15) right, or should verification get more on heavy-redteam days? (The override endpoint makes this adjustable without a deploy.)
2. Should the cron class share its budget with the `overhead` bucket, or should overhead get its own 4th class? (Design assumes shared; flag if overhead proves noisy.)
3. Confirm current D1 free-tier write limit (100K/day used here from documentation — verify before implementing).
