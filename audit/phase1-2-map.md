# GEPPETTO HARD — Phase 1-2: System Map + Authority Graph
**Agent A (recon) · 2026-10-06 · Source: `~/workspace/exchange/worker.js` (2053 lines, v0.6.0) · Verified live**

## 1. System map

**Processes:** 1 Cloudflare Worker (V8 isolate) + D1 (SQLite, single-writer) + cron (5min).
No other processes. Workers (nim-claimant, fake-claimant) are external OS processes.

**Network boundaries:**
- Internet → Worker: HTTPS. `/`, `/v1/health` open. `/v1/workers/register`, `/v1/claim`,
  `/v1/complete`, `/v1/admin/sweep-expired-claims` before Exchange-key gate (worker-scoped auth).
  Everything else: `Authorization: Bearer <EXCHANGE_KEY>` or 401.
- Worker → D1: `env.EXCHANGE_DB` binding (same account).
- Worker → providers: outbound fetch (adapters). Claimed adapter: **no outbound** (never probes).
- Worker scripts → vault: local surrogate (`dynamic_credentials`), never over network to Exchange.

**Auth boundaries (verified live):**
| Caller | Credential | Reach |
|---|---|---|
| Anyone | none | `/`, `/v1/health` |
| Worker | `X-Worker-Id` + `X-Worker-Secret` (sha256 stored) | claim, complete only |
| Governor admin | `Bearer EXCHANGE_KEY` | all routes incl. workers/register, admin/sweep, observe |
| Worker | — | **cannot** reach observe, policy, seed, discover, job submit |

Live-verified: no-auth claim → 401; bad secret → 403; remote worker on `nim-nvidia`
(local-trust) → 403 `"resource requires local-trust claimant"`.

**Persistence:** D1 tables: `jobs`, `leases`, `claims`, `executions`, `receipts`,
`accounting`, `resources`, `providers`, `workers`, `quotas`, `health_observations`,
`evidence_log`, `policies`, `resource_capabilities`, `discovery_sources`.
No transactions used anywhere (no `batch()`); all multi-write flows are sequential
single statements. D1 single-writer serializes statements, not requests.

## 2. Traced paths (WHO / WHAT / EVIDENCE / BLOCKS / DEATH)

### 2.1 Discovery — `registerCandidate` (route `/v1/resources`, `/v1/discover`; Exchange key)
- WHO: governor admin (master key).
- WHAT: INSERT provider (or IGNORE) + INSERT resource as DISCOVERED + capabilities + evidence_log.
- EVIDENCE: none required — registration is a lead, explicitly unusable.
- BLOCKS: master key gate. Unknown adapter rejected (`adapters[cfg.adapter]` lookup).
- DEATH: between provider INSERT and resource INSERT → orphan provider row, harmless
  (providers are never routed to; only resources are).

### 2.2 Worker auth — `authWorker`
- WHO: worker presenting id+secret.
- WHAT: SELECT worker, compare `sha256hex(secret)` to stored hash, UPDATE last_seen_at.
- EVIDENCE: secret hash match. Secrets stored hashed; shown once at register.
- BLOCKS: 401 missing, 403 unknown/disabled/bad-secret.
- DEATH: n/a (single read + one UPDATE; last_seen_at loss is cosmetic).
- NOTE: no rate limiting / lockout on failed attempts. Secret entropy ~128 bits
  (`wsec_` + 2×uid) makes brute force impractical. Labeled PLAUSIBLE-safe, not PROVEN.

### 2.3 Verification — delegated gauntlet (`/v1/resources/:id/verify`, claimed adapter)
- WHO: governor creates verification job → `delegateExecution`; worker executes via claim/complete.
- WHAT: job → deferred → claim → execution → completion → observations feed ladder.
- EVIDENCE: every tier earned from recorded observations/receipts; `warrantedTier`
  requires ok-observations + ok-executions. No shortcuts in code path.
- BLOCKS: evidence gate (EVIDENCE_USABLE) until promoted; trust gate on claim.
- DEATH: verification job deferred with active lease, no claim → cron replan skips
  while lease live (`l.expires_at > datetime('now')`); after expiry replans. No stuck state.

### 2.4 Gate evaluation — `govern` (10 gates)
- WHO: governor, in-request, deterministic.
- WHAT: no state changes except INTEGRITY revalidation probe → `observe()` (writes
  health_observations + maybe evidence_log). All else read-only; trace recorded.
- EVIDENCE: D1 rows (resources, quotas, policies, observations).
- BLOCKS: gate order fixed; unreached gates labeled "vacuous", never faked as passed.
  AUTHORITY gate is a record (`"bearer key valid"`) — the real check is `authed()`
  at the route layer. **A route placed before the gate without its own auth check
  would bypass AUTHORITY silently.** Current pre-gate routes all auth (worker or master).
- DEATH: govern is read-only (except integrity probe) → safe.

### 2.5 Lease creation — `createLease`
- WHO: governor (`runJob`, `delegateExecution`).
- WHAT: INSERT lease (state=active, 60s expiry; delegation extends to 10min),
  authority=`"bearer:exchange-key"`.
- EVIDENCE: job passed gates; policy_version recorded.
- BLOCKS: only callable from governor code paths (not a route).
- DEATH: lease row without job update → orphan active lease; cron replan treats as
  live until expiry, then replans. Self-healing within ~10min.

### 2.6 Claim — `/v1/claim` (worker auth)
- WHO: authenticated worker; governor executes the claim logic.
- WHAT: SELECT oldest unclaimed delegation → conditional
  `UPDATE leases SET state='claimed' WHERE id=? AND state='active'` → check
  `meta.changes` → INSERT claim row → INSERT execution row (result='running') →
  return delegation object (11 fields; `credential_ref` opaque, verified live:
  `vault:custom.nvidia-nim`, no secret material in payload).
- EVIDENCE: active lease + deferred job + no existing claim + trust_class sufficient.
- BLOCKS: worker auth; trust gate (`claimant_trust_required=local` → 403 for remote);
  atomic UPDATE (single statement; W1 proven — exactly one winner in thread race).
- DEATH: **FINDING — orphan claimed lease.** If the process dies after the UPDATE
  but before the claim INSERT, the lease is `state='claimed'` with no claim row.
  The claim query requires `l.state='active'` → never re-pickupable.
  `completeClaimed` requires claim_id → never completable.
  `sweepExpiredClaims` only scans `claims` table → never swept.
  **This is a stuck state with no recovery path.** Probability low (single-digit-ms
  window between two statements), impact permanent for that job. The job row stays
  `deferred`; cron replan skips it while `l.expires_at` is future (live check sees
  `state='claimed'` + unexpired), and after expiry the live check
  (`l.expires_at > datetime('now')`) fails → replan proceeds and creates a NEW
  lease. So the job recovers via replan after ~10min, but the orphan lease row
  persists forever. Labeled STRONGLY SUPPORTED (code-traced; not live-reproduced).

### 2.7 Execution (inline) — adapter.execute in `runJob`
- WHO: governor (Worker isolate) executes provider fetch directly.
- WHAT: fetch → `observe()` → `quotaConsume()` → lease consumed/released →
  execution row updated → VERIFY content → receipt.
- EVIDENCE: provider HTTP response.
- BLOCKS: lease revalidation immediately before dispatch (TOCTOU guard):
  re-reads resource state, evidence, throttle, cost, quota. Any change → invalidate,
  `continue` to next candidate.
- DEATH: provider executed but Worker died before receipt → execution row stays
  `running`, lease stays `active`→ expires; job stays `running` → orphan reaper
  (10min, no receipt) → deferred → replan. **Provider may have executed; replan
  re-executes. Double-execution possible, honestly recorded as separate attempts.**

### 2.8 Execution (delegated) — worker process
- WHO: external worker (untrusted).
- WHAT: outside governor visibility. Only the 5 terminal outcomes cross the boundary.
- EVIDENCE: none until `/v1/complete`.
- BLOCKS: 10-min lease; completion validates everything governor-side.
- DEATH: zombie sweep → EXPIRED, execution UNCERTAIN, job deferred. W5 proven.

### 2.9 Completion — `completeClaimed` (worker auth)
- WHO: worker reports; governor decides everything material.
- WHAT (in order): validate claim+worker binding → forgery checks → idempotency
  check → expiry check → outcome whitelist → `observe()` → `quotaConsume()` →
  429-ceiling → model binding → independent output verification → quarantine
  decision → **accounting INSERT** → execution UPDATE → lease settle →
  job state → receipt INSERT → claim marked completed.
- EVIDENCE: claim row; worker_outcome is evidence, taxonomy/verification are governor's.
- BLOCKS: 404 unknown claim; 403 cross-worker; 400 resource/job mismatch or bad
  outcome; 409 expired/completed-replay (idempotent 200, no double accounting —
  W2 proven); cost from `costOf(resource)`, worker `cost` field ignored (W6 proven).
- DEATH: **FINDING — partial persistence.** ~15 sequential writes, no transaction.
  Die after accounting INSERT but before receipt → books have a row the receipt
  system can't see (receipt verify's `accounting_bound` check would fail open —
  it checks `!!ac`, missing row → check fails → receipt unverifiable, which is
  honest but the money-trail is split-brained). Die after receipt but before
  claim UPDATE → replay returns non-idempotent path and could double-account
  (mitigated: receipt exists, but claim still `active` → second completion
  proceeds → second accounting row). Window is ms; D1 has no rollback, so partial
  state is permanent. Labeled STRONGLY SUPPORTED.

### 2.10 Completion vs sweep race — FINDING
- `completeClaimed` checks `claim.status='active'` then acts; `sweepExpiredClaims`
  SELECTs active-expired claims then `expireClaim`s. Two concurrent requests can
  interleave: complete passes the check, sweep expires the claim, complete writes
  success receipt + accounting on an expired claim — or sweep marks `uncertain`
  after complete marked `ok`. Last-writer-wins on `executions.result`.
  No serialization beyond single-statement atomicity. W2 tested sequential replay
  only; concurrent complete-vs-sweep is UNTESTED. Labeled PLAUSIBLE (narrow window,
  requires expiry-boundary timing).

### 2.11 Receipt creation
- WHO: governor only. No route writes receipts except via governor code paths.
- WHAT: INSERT with full trace, lease snapshot, quota/cost/health before-after.
- EVIDENCE: the trace itself + input_hash recomputation.
- BLOCKS: verify route (`/v1/receipts/:id/verify`) recomputes input_hash, checks
  gate completeness (following the delegation chain for delegated completions),
  `claim_binding` (claim completed by the receipt's worker), `accounting_bound`
  (accounting row exists, measured_cost == receipt cost).
- DEATH: receipt INSERT is near-last in each flow; earlier death → no receipt →
  reaper/replan treats as not-happened. Receipts are never partially written
  (single INSERT).

### 2.12 Accounting — `accounting` table
- WHO: governor only, in `completeClaimed` (delegated) and inline path (receipt
  cost fields; inline has no accounting row — **FINDING: asymmetry**).
- WHAT: INSERT (id, execution_id, claim_id, job_id, resource_id, measured_cost,
  cost_basis, quota_effect).
- EVIDENCE: `costOf(resource)` + resource.cost_basis. Worker assertions ignored.
- BLOCKS: no worker route touches it; `billingCheck` only demotes when
  `cost_class='free'` (paid-execution fix live).
- **FINDING: inline executions write cost into the receipt but never into
  `accounting`.** The `accounting_bound` verify check only applies to delegated
  receipts (`rcpt.claim_id ?`). Two bookkeeping systems: receipts.cost (both
  paths) vs accounting rows (delegated only). A "total measured spend" query must
  UNION both. Labeled PROVEN (code + live: inline job receipts have cost fields,
  zero accounting rows).

### 2.13 Capacity update — evidence ladder
- WHO: cron (`autoPromote`/`demoteCheck`), `observe()` rollups, verify gauntlet.
- WHAT: `evidence_state` DISCOVERED→…→VERIFIED; `state` AVAILABLE/DEGRADED/
  THROTTLED/QUARANTINED/EXHAUSTED.
- EVIDENCE: `warrantedTier` counts ok-observations (24h/7d) + ok-executions.
  Workers cannot write evidence except via completion → `observe()`.
  `/v1/observe` requires master key (line 1957, after gate) — workers cannot
  inject observations directly. PROVEN (route placement + live 401 without key).
- BLOCKS: `setEvidence` refuses demotion without `force=true`; promotion only
  via `warrantedTier`.
- DEATH: cron is idempotent per resource; safe.

## 3. Authority graph — boundary crossings

```
MASTER KEY ──→ routes: seed, resources, discover, job, observe, admin, workers/register
   │  (never reaches worker; verified: claim/complete use worker creds only)
WORKER CREDS ──→ claim, complete (scoped; per-worker; hashed at rest)
   │  (cannot reach observe/policy/job-submit; 401 live-verified)
VAULT ──→ worker process only (surrogate; never Worker, never D1, never delegation)
   │  delegation carries credential_ref (opaque string, verified live)
D1 ──→ single-writer; statements atomic, requests NOT (no transactions)
CLOCKS ──→ 3 clocks: Worker Date.now() · D1 datetime('now') · worker-script local
```

**Edges where authority crosses:**
1. `workers/register` (master key → mints worker identity). Authority: governor admin.
   No rotation/revocation route except `disabled` flag (no route sets it — **FINDING:
   revocation is D1-manual only**).
2. `claim` (worker creds → governor creates claim+execution rows for the worker).
   Authority stays governor; worker only selects resource_id.
3. `complete` (worker outcome → governor writes books). Worker input is evidence.
4. `observe` via completion (worker success/failure → health rollup → quarantine).
   A lying worker can poison a resource's health (3 consecutive failures →
   QUARANTINED). **FINDING: health rollup trusts worker-reported outcomes.**
   Mitigation: observations are labeled `claimant:`; quorum is per-resource.
   A single hostile worker with claim access can QUARANTINE a resource in 3
   completions. Labeled STRONGLY SUPPORTED (code-traced).
5. Cron (time → authority): sweep, replan, demote, billing. No auth (internal).

**Credentials crossing:** none. Worker secret travels once (register response,
TLS). Vault key never leaves VM. `secret_hash` is sha256 (not salted — **NOTE:
   unsalted sha256; rainbow-table risk is negligible for 128-bit random secrets**).

**Durability:** every material transition writes D1 before responding, except
in-request computed values (scores, traces) which are persisted in receipts.

**Replay:** claim (safe reject: claimed:false / lost-race); complete (idempotent
on claim_id — sequential); register (409 on duplicate); job submit (dedupe on
idempotency_key → job_id). **Concurrent** complete and complete-vs-sweep: see 2.10.

**Races:** W1 (solved, atomic UPDATE). 2.6 orphan lease (stuck-ish, self-heals via
replan). 2.9 partial writes. 2.10 sweep race.

**Ambiguity:** `executions.result` values: ok/error/running/uncertain (+ legacy
'claimed'). `uncertain` is terminal-ish (sweep writes it; nothing transitions
it further — **FINDING: UNCERTAIN has no outgoing transition**; a later
successful replan creates a NEW execution, leaving the uncertain row as history.
That matches doctrine).

## 4. Clock analysis — FINDING
- Lease expiry checked 3 ways: `completeClaimed` uses `Date.parse(claim.lease_expiry)
  < Date.now()` (Worker clock); `sweepExpiredClaims` uses `lease_expiry <
  datetime('now')` (D1 clock); cron live-check uses `l.expires_at > datetime('now')`
  (D1 clock); worker scripts use local clock for pre-validation.
- `ts()` parses D1 datetimes as UTC (correct: D1 `datetime('now')` is UTC).
- Worker `now()` emits ISO with `Z` (UTC). Consistent.
- **Skew risk:** Cloudflare Worker clock vs D1 clock are both NTP-synced infra
  clocks; skew is ms-scale. The claim/complete window is minutes. Impact:
  negligible in practice, but the *semantic* expiry authority is split across two
  clocks with no canonical definition. Labeled INFORMATIONAL.

## 5. What is PROVEN vs UNKNOWN (this recon)

PROVEN (live): worker auth boundaries (401/403 matrix); trust gate (403 remote→
local); delegation shape (11 fields, opaque credential_ref, no secret material);
atomic claim (W1); idempotent replay sequential (W2); forgery rejection (W3);
expiry rejection (W4); zombie sweep → UNCERTAIN (W5); cost-assertion ignored (W6);
no manufactured success (W8); model binding (W9); zero secret leaks (W7);
FIFO claim order (oldest lease first); policies immutable (seed-only).

STRONGLY SUPPORTED (code-traced, not live-fired): orphan claimed-lease (§2.6);
partial-write in completeClaimed (§2.9); health-rollup poisoning by hostile
worker (§3.4); accounting asymmetry inline vs delegated (§2.12).

PLAUSIBLE (narrow/untested): concurrent complete-vs-complete; complete-vs-sweep
race (§2.10); auth brute-force economics (§2.2).

UNKNOWN: behavior under D1 partial-commit (D1 doesn't expose it); Worker isolate
crash mid-request recovery semantics; whether `disabled=1` workers are rejected
at claim (code says yes — `!w || w.disabled` → 403 — actually PROVEN by code,
untested live).

## 6. Deletion candidates (§13 license)
- `evidence_at`/`evidence_source` columns are write-mostly; nothing reads them
  except the evidence route display. Keep (cheap, auditable).
- `discovery_sources` + `runDiscovery`: **zero sources configured live**
  (`SELECT COUNT(*)` → 0, verified). The cron's discovery leg iterates an empty
  set every 5 minutes — dead path, never exercised. Candidate for deletion or
  for actually configuring a source.
- The `self` adapter's `probe()` returns ok unconditionally — `exchange-self`
  is VERIFIED by assertion in backfill, not by probing. Noted, not condemned.
- `spend_authority` is plumbed through policy_snapshot but no paid resource
  exists live; the COST gate logic is untested against real paid flow. UNKNOWN.

## 7. Handoff to attack agents
Priority targets: §2.6 orphan lease (construct it: kill between UPDATE and
INSERT — suggest fault-injection via a test hook, not prod); §2.9 partial writes
(complete with a worker that reports then check intermediate states — hard to
time; consider a proxy that delays); §2.10 concurrent complete-vs-sweep
(spawn both simultaneously at the expiry boundary); §3.4 health poisoning
(3 hostile completions → QUARANTINED, then verify governor recovers);
§2.12 accounting UNION query correctness.
Constitutional: none of these require changing v1 — they test around it.
