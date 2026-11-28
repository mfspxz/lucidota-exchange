# D1 Read-Burn Fix — Worklist A3 (2026-11-22)

**Status: attributed, patches ready, NOTHING applied, NOTHING deployed.**
Production `worker.js` and the Rust port under `rs/` are byte-untouched
(verified by mtime after this audit). The two patches are standalone
files next to this report:

- [D1-BURN-FIX-A3.worker-js.patch](sandbox://workspace/exchange/D1-BURN-FIX-A3.worker-js.patch) — for production `worker.js` v0.7.3
- [D1-BURN-FIX-A3.rust.patch](sandbox://workspace/exchange/D1-BURN-FIX-A3.rust.patch) — for `rs/exchange-core` (migrate.rs + routes.rs)

Both patches were validated mechanically: applied to copies in /tmp,
zero fuzz, results byte-identical to the reference builds, patched
worker.js passes `node --check`, patched Rust files parse clean.

Prior art read in full before measuring: the culprit-hunt goal dir —
GOAL.md, `GOVERNOR-REMEDIATION-WORK-ORDER-2026-11-20.md`,
`EVIDENCE-LADDER-DEEP-DIVE-2026-11-20.md`,
`PHASE3-EVENT-DRIVEN-PROMOTION.patch`, the 2026-11-21 post-reset
promotion check, and the 05:50-window evidence extract.

---

## 1. The burn, measured live (2026-11-23 ~05:17–05:40Z)

Source: Cloudflare GraphQL analytics `d1AnalyticsAdaptiveGroups`,
database `0ea89801-9b0a-4efa-9d2f-170d7d285e69` only, plus
`workersInvocationsAdaptive` for script `lucidota-exchange`, plus
per-query `meta.rows_read` replays via `cf.py d1q` / `d1q-full.py`.

### Daily rows read, this database

| Date | Read queries | Rows read | Rows written |
|---|---|---|---|
| 2026-11-16 | 29,646 | 1,011,520 | 5,326 |
| 2026-11-17 | 35,608 | 1,135,554 | 8,036 |
| 2026-11-18 | 35,761 | 1,159,781 | 7,830 |
| 2026-11-19 | 38,771 | 1,298,584 | 7,990 |
| 2026-11-20 | 119,628 | 4,992,065 | 5,663 |
| 2026-11-21 | 542,004 | 5,083,958 | 14,928 |
| 2026-11-22 | 492,052 | 5,492,785 | 9,181 |
| 2026-11-23 (to ~05:20Z) | 153,175 | 5,700,262 | 5,255 |
| 2026-11-23 (to ~05:40Z) | 163,552 | 7,154,324 | — |

The only other database on the account read 3,643 rows on Nov 22 and
1,128 on Nov 23. **The Exchange database is 99.9% of the account's D1
read volume.** This is not bot traffic, not another worker, not the
mirror — it is this database's own workload.

### The exhaustion signature (2026-11-22, hourly)

00:00–08:00Z: 14.3–19.2k read queries/h, 500–675k rows/h. Cumulative
rows cross ~5.24M by the end of the 08:00 hour; the 09:00 hour is
partial (249,922 rows). **From 10:00Z to midnight: read queries
continue at 19.7–26.2k/h while rows read collapse to 0–211/h.** The
queries keep arriving and fault on the quota; nothing backs off. The
~09:20Z daily death in the task brief matches this curve.

### Traffic volume (worker invocations)

Nov 22: **12,940 requests total, 505–580/h, flat across all 24 hours**
— including the dead hours. Nov 23 00:00–05:00Z: 2,945 (~550/h). The
request rate never changes; only the cost per request does.

---

## 2. Attribution

### 2.1 Who generates the traffic — the claimant fleet (hypothesis: PROVEN)

From source, not logs:

- `~/hooks/definitions/nim-claimant.json`: `poll_interval_secs: 120`.
- `~/hooks/scripts/nim-claimant.sh`: each run launches
  `phase3-claimant.py` once per credential file — 4 exist
  (`~/.exchange_worker_nim` + `_01.._03` pattern files; counted, never
  read) — **plus one claimant per additional key resource** (groq,
  cerebras, mistral, cohere, openrouter, deepseek, friendli, zen,
  cloudflare-ai, ai-router) = **14 claimant processes per run**.
- Each idle claimant makes exactly one `POST /v1/claim` and exits
  (drain loop breaks on the first `claimed:false`).
- `mint-claimant` hook: 1 claimant per 300s = 12 polls/h.

Arithmetic: 14 × 30 runs/h = **420 claim POSTs/h**, + 12 mint + 2 cron
+ health/job/misc ≈ the measured ~540 requests/h. **Claim polls are
~78% of all Exchange traffic.** The standing hypothesis named the
right fleet.

### 2.2 What one request costs — replays (meta.rows_read, production D1)

| Query (exact worker SQL) | Rows read |
|---|---|
| `authWorker`: `SELECT * FROM workers WHERE worker_id=?` | 1 |
| claim: `SELECT * FROM resources WHERE id=?` | 1 |
| claim: unclaimed-delegation join (nim-nvidia) | 32 |
| **Idle claim total** | **34 rows, 3 read queries** |
| `PRAGMA table_info(...)` (×47 in migrate) | 0 each |
| migrate tail: `SELECT id FROM ballot_policy WHERE id='default'` | 1 |
| migrate tail: `SELECT COUNT(*) FROM evidence_log WHERE trigger='backfill-v0.3'` | **3,156** |
| `/v1/health` (COUNTs: resources + model_capabilities + key_config) | 42 + 656 + 11 = 709 |
| `warrantedTierInfo` for nim-nvidia (state + ok7 + ok24 + execOk 592 + freeExec 1,185 + quota) | 1,789 |

Table sizes at measurement: health_observations 30,226; evidence_log
3,156; receipts 1,838; executions 813; jobs 818; leases 838; claims
760; model_capabilities 656; resources 42; workers 14.
**evidence_log has no index** (38 indexes listed; none on
evidence_log), so the backfill COUNT is a full scan — and evidence_log
is append-only, so the scan grows every day.

### 2.3 The mechanism — per-cold-isolate migrate() (PROVEN, share fitted)

`handle()` calls `ensureMigrated(db)` on every request (worker.js
:2502). The v0.6.13 guard runs `migrate()` once **per isolate** — and
migrate() is ~84 statements (47 `ensure()` PRAGMA checks, ~31
`CREATE ... IF NOT EXISTS`, policy/backfill checks) ending in the
3,156-row evidence_log scan. Total cold-isolate cost ≈ **84 read
queries / ~3,157 rows**, versus 3 queries / 34 rows for the claim
itself — a ~93× multiplier on rows.

Sparse polling (~540 req/h spread across Cloudflare's edge) churns
isolates, so a large share of polls pay it. Fitting the measured
analytics on **both axes simultaneously** with the cold-isolate share
as the single free parameter:

| Day | Measured queries/req | Measured rows/req | Fitted cold share | Model check |
|---|---|---|---|---|
| Nov 22 (live hrs) | 28–35 | ~1,020–1,250 | ~0.34 | 4 + .34×80 ≈ 31 q; 34 + .34×3,157 ≈ 1,107 r ✓ |
| Nov 23 (00–05Z) | ~53 | ~1,960 | ~0.61 | 4 + .61×80 ≈ 53 q; 34 + .61×3,157 ≈ 1,960 r ✓ |

The mechanism is proven in source and the fit closes on both axes on
two independent days. The cold-isolate share itself is fitted, not
directly observable from outside; **why it roughly doubled overnight
Nov 22→23 is UNKNOWN** (isolate placement is Cloudflare-internal).
The v0.6.13 changelog records this exact failure mode from October —
migrate-per-request "blew 75% of daily 5M rows_read" — the isolate
guard only partially fixed it, because the guard's unit (the isolate)
is not durable under this traffic shape.

**Verdict on the standing hypothesis:** the nim-claimant fleet is the
traffic source (PROVEN), but the polls are not the cost — an idle
claim is 34 rows, ~1% of the fitted burn. **~99% of the burn is
migrate() re-running on cold isolates, triggered by the polls.** The
hypothesis named the right fleet and the wrong cost center.

### 2.4 Step-changes line up with the estate's own changes

- **Nov 20 (queries ×3.4, rows ×4):** v0.7.0 deployed — migrate()
  grew (model_capabilities, key_config, config_log, exchange_config
  DDL + seeds), and the Phase 3 promotion hook went live.
- **Nov 21 (queries ×14 vs baseline):** the nim-claimant hook's
  multi-resource drain landed (hook script header, dated 2026-11-21):
  4 → 14 claimant processes per run.

### 2.5 Exonerated, with numbers

- **The `*/30` cron / scheduled():** replayed `warrantedTierInfo`
  exactly as scheduled() computes it. Claimed resources total
  **2,479 rows/run** (nim-nvidia 1,789 of it; the other 14 claimed
  resources 3–179 each); even computing it for all 42 resources would
  be 4,347. With base run costs, ≈ ~3k rows/run ≈ **~145k rows/day ≈
  3% of the cap**. The wave-29/v0.6.15 remediations (memoization,
  fresh-path skip, retention pruning) worked. Phase 3.2 (shrink
  scheduled()) is therefore low-value — see §4.
- **`/v1/health`:** 709 rows/call, but call frequency is monitors
  only. Not the burner.
- **PRAGMAs:** rows_read = 0 measured. They inflate the query *count*
  inside migrate but burn no rows.
- **Other databases / other workers:** 0.1% of account rows (§1).

### 2.6 No backoff after death (secondary defect)

After the Nov 22 cap bit (~09:20Z), the fleet kept polling at the
same fixed 120s cadence for 14 more hours — 20–26k faulting read
queries/h accomplishing nothing (rows ≈ 0). The claimant treats a
quota-dead Exchange as a transient failure and the hook's cadence is
fixed; there is no circuit breaker anywhere in the loop. (Why the
post-cap query *rate* is higher than the live rate at equal request
volume is UNKNOWN — noted, not explained.) The A3 patches shrink the
burn so the cap is not hit; a claimant-side backoff on repeated
D1_ERROR-shaped claim failures is a separate, smaller follow-up —
flagged here, not patched, because it changes claimant behavior
rather than read cost.

### 2.7 One honest discrepancy

On Nov 23 the analytics total passed 5M rows by ~04:40Z and 7.15M by
~05:40Z, yet the worker was still answering (`/v1/health` 200 with
live counts at 05:17Z and 05:26Z, and API queries succeeded
throughout). On Nov 22, enforcement visibly bit at ~5.24M cumulative.
So the enforced quota's accounting is **not identical** to the
analytics `rowsRead` total, or enforcement lagged today. Which is
true is UNKNOWN from available evidence. It does not change the fix:
at the current burn rate the DB is at the cap's mercy every day,
whatever the exact accounting.

---

## 3. The fix

Design rules for both patches: no behavior change, no evidence
semantics change, fail-open toward today's behavior on any doubt,
markers shared across implementations because the flip runs both
against one D1.

### 3.1 worker.js patch — `D1-BURN-FIX-A3.worker-js.patch`

1. **Schema-epoch gate** (`ensureMigrated`): before paying for
   migrate(), one PK lookup — `SELECT value FROM exchange_config
   WHERE key='schema_epoch'` (1 query, 1 row). Full migrate only when
   the epoch is stale or unreadable (pre-v0.7 DBs have no
   exchange_config; the error path runs migrate exactly as today).
   migrate() completion stamps `schema_epoch='v0.7.3-a3-1'`. A loud
   comment binds the epoch to any future schema change.
2. **Backfill marker** (inside migrate()): the evidence_log COUNT is
   replaced by the `backfill_v03_done` marker (PK read). The COUNT
   runs at most once more, on the first post-patch migrate, and
   writes the marker either way.
3. **`idx_evidence_log_trigger`** added in migrate(), so future
   trigger scans are indexed.

Cold-isolate cost: ~84 queries / ~3,157 rows → **2 queries / 2 rows**.

### 3.2 Rust patch — `D1-BURN-FIX-A3.rust.patch` — **PRE-FLIP REQUIREMENT**

The port is worse than production on this axis: `handle_inner` calls
`migrate(db)` **unconditionally on every request** (routes.rs:314) —
there is no isolate guard at all — and migrate.rs:247 reproduces the
same unindexed backfill COUNT. Flipping as-is would cost the full
~3,157 rows on **every** request: ~1.7M rows/h at current traffic,
cap gone by ~03:00Z. The patch mirrors the JS fix exactly (same
`SCHEMA_EPOCH = "v0.7.3-a3-1"`, same marker keys):

1. `ensure_migrated()` in migrate.rs: per-isolate `AtomicBool` fast
   path, else the same one-row epoch gate; `SCHEMA_OK` set only on
   success so a failed migrate retries next request (the JS
   `__migrating` reset semantics).
2. Both call sites (handle_inner, scheduled) use it.
3. Backfill marker + `idx_evidence_log_trigger`, identical to JS.

### 3.3 Projected post-fix burn (at current traffic)

| Source | Rows/day |
|---|---|
| Claim polls (10,080/day × 34) | ~343k |
| scheduled() (48 × ~3k) | ~145k |
| Completions / gauntlets / job submits (workload-bound) | tens of k |
| Health + misc | <10k |
| **Total** | **~0.5M/day ≈ 10% of the 5M cap** |

### 3.4 Deliberately NOT in the patches

- **warrantedTierInfo lifetime COUNTs** (execOk 592 + freeExec 1,185
  index rows for nim-nvidia, growing with history): windowing them
  to 30 days is deep-dive item **G4 — a semantics decision for
  Zack**, not a burn patch. At ~3% of daily burn via the cron plus
  per-completion calls, they are not what kills the day.
- **Claimant backoff** (§2.6): real, smaller, behavior-changing —
  separate follow-up.

---

## 4. Correction to the Phase 3 draft

The goal dir's `PHASE3-EVENT-DRIVEN-PROMOTION.patch` is **stale**:
its hunk is already live in production — v0.7.0 folded it in
(worker.js:1276–1277, `autoPromote`/`demoteCheck` inside
`completeClaimed`, and the v0.7.0 changelog line says so). Do not
apply it; it would double-apply. Its follow-up item 3.2 (shrink
scheduled()) is measured in §2.5 at ~3% of the burn — no longer a
priority. The draft file stays as the historical record.

---

## 5. Apply + verify (for whoever lands this — not done here)

JS: `cd ~/workspace/exchange && patch -p0 < D1-BURN-FIX-A3.worker-js.patch`,
deploy via the existing `deploy-exchange-070.py` path.
Rust: apply inside `rs/`, `cargo test --release --offline` (incl.
v07_parity — fresh-DB tests still pass: the first migrate runs the
COUNT once, honors the seeded marker rows, stamps the config marker),
and land it **before or with** the flip, never after.

Verification (receipts, per the work order's standard):
1. `/v1/health` counts unchanged (42 / 656 / 11 at measurement).
2. `SELECT key,value FROM exchange_config` shows
   `schema_epoch='v0.7.3-a3-1'` and `backfill_v03_done='1'`.
3. One hour of post-deploy analytics: read queries/h and rows/h fall
   off the ~29k / ~1.08M plateau toward ~2–3k / ~40–80k.
4. The worker survives past 09:20Z the next day — no D1_ERROR on `/`,
   `/v1/health`, or a claimant cycle.
5. Rollback: restore pre-patch worker.js / rs files, redeploy. No D1
   rollback — both markers are inert config rows and the index is
   additive; the unpatched implementation ignores the markers, the
   patched one honors them, in either order.

## 6. Measurement disclosure

All D1 access in this audit was read-only SELECT/PRAGMA via the
estate's own helpers (`cf.py d1q`, `d1q-full.py`) and read-only
GraphQL analytics — roughly 300 API queries total, ~10k rows of
replay reads. No writes, no deploys, no secret values read or
printed (credential *files* were counted by name pattern only).
