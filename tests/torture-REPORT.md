# Torture report — exchange worker.js v0.6.16 (post-hardening)

Campaign: Zack's order 2026-10-10 "torture my codebase". Attacker-only:
no worker.js changes, no fixes, new tests only (`tests/torture-*.mjs`).
Trinary verdicts per vector: **BROKEN** (minimal reproduction) /
**HOLDS** (evidence) / **UNKNOWN** (blocked + why).

Test method: the REAL worker.js against `tests/d1-shim.mjs`
(node:sqlite D1 facade, node v24). Fresh DB + fresh module isolate per
vector group. Run: `node tests/torture-prune.mjs`,
`node tests/torture-complete.mjs`, `node tests/torture-seal.mjs`,
`node tests/torture-envelope.mjs`.

**Score: 34 vectors HOLDS, 2 BROKEN, 3 UNKNOWN (shim-limited).**
No RCE-adjacent, auth-bypassing, or secret-leaking findings.

---

## BROKEN 1 — sweep-interleaved double-execution (C3)

**Severity: medium (integrity). Exploitability today: LOW (latent).**

`completeClaimed` performs all non-idempotent side effects (health
`observe`, `quotaConsume`, `accounting` INSERT with fresh `uid`,
completion receipt INSERT with fresh `uid`, ternary/prune logic)
*after* the atomic active→completing flip, but only the *final*
`UPDATE claims SET status='completed' ... WHERE status='completing'`
is conditional. If `sweepExpiredClaims` fires between the flip and the
final write — it resets `status='completing'` → `'active'` for claims
whose **`claimed_at` is older than 15 min** — the in-flight completion
keeps writing (its final UPDATE no-ops against `'active'`), returns
200, and a worker retry then executes the *whole completion a second
time*. The idempotency guard (`status='completed'` early-return) never
fires because attempt #1 was never marked completed.

**Reproduction** (`tests/torture-complete.mjs` C3, deterministic via
instrumented DB that widens the flip→final-write window):
1. Claim `cX` active, `claimed_at = now-20min`, `lease_expiry = now+1h`.
2. `POST /v1/complete` → flips to `completing`.
3. `POST /v1/admin/sweep-expired-claims` → `RESET_COMPLETING_TO_ACTIVE`.
4. Attempt #1 finishes → **200** (final write no-op, claim left `active`).
5. Retry with a fresh envelope → **200**, full second execution.
6. Result: **2 receipts + 2 accounting rows for one claim_id.**

**Reachability caveat (honest):** the stuck-sweep keys on `claimed_at`
(claim-creation time), and standard lease paths are 60s (`createLease`)
or 10min (delegation pickup) — both under the 15min threshold — so a
legitimate >15min-old claim with a still-valid lease is *not reachable
via standard paths today*. No admin lease-extension endpoint exists and
nothing rewrites `claimed_at`. The flaw is **latent**: any future
>15min lease path reopens it silently, and nothing in code enforces
lease ≤ 15min. Cheap fix: re-check `status='completing'` (or stamp a
flip token) before the non-idempotent side effects, or key the
stuck-sweep on flip time rather than claim age.

Related vectors that HOLD: C4 (sweep leaves *fresh* in-flight
completions alone), C5 (designed crash-recovery: sweep reset →
complete 200s → late retry idempotent-replays, exactly 1 receipt),
C2 (concurrent double-complete → 200+409, exactly 1 receipt +
1 accounting row).

---

## BROKEN 2 — migrate() cold-start race → transient 500s (P7)

**Severity: low-medium (availability). Exploitability: MEDIUM during
schema-change deploys.**

`handle()`: `if (!__migrated) { await migrate(db); __migrated = true; }`
— the flag flips only *after* `migrate()` resolves, so N concurrent
requests on a fresh isolate all enter `migrate()`. Its `ensure()`
helper is check-then-act (`PRAGMA table_info` → `ALTER TABLE ADD
COLUMN`); the losers throw `duplicate column name: evidence_state` →
caught by the outer handler → **500 "exchange fault"**.

**Reproduction** (`tests/torture-prune.mjs` P7): 5 rounds × 4
concurrent `/v1/observe` on a fresh isolate → **15 of 20 requests
500'd** with `duplicate column name`. Self-heals on retry (next
request sees the columns present; `__migrated` then sticks).

**Reachability:** in production the migrate-added columns already
exist on D1, so `ensure()` no-ops — the race only bites when a deploy
adds *new* columns and a traffic burst hits cold isolates, i.e.
exactly the deploy+spike moment. Narrow window, transient, no data
corruption (the losing ALTER fails clean). Cheap fix: set a
module-level `migrating` promise *synchronously* before the first
await, or make `ensure()` tolerate duplicate-column errors.

Note: this race polluted early torture runs (concurrent tests on fresh
isolates); all other vectors warm the isolate first to test their own
logic cleanly.

---

## HOLDS — disproof corroboration quorum (Finding 1 re-attack)

v0.6.16: `COUNT(DISTINCT c.worker_id)` over stamped proposals joined
to eligible (registered, not-disabled) workers; the current worker's
unstamped proposal counts iff it has not already proposed for this
`input_hash`; prune is one D1 batch with conditional UPDATEs +
deterministic `INSERT..ON CONFLICT DO NOTHING`.

- **P1**: sequential 3-worker quorum — w1: no prune; w2: prune, exactly
  1 receipt; w3: still exactly 1 receipt. HOLDS.
- **P2** (disabled-worker resurrection): w1 proposes, w1 disabled →
  w2's -1 does NOT prune (vote not resurrected); w1 re-enabled →
  w3's -1 prunes (live eligibility, 2 distinct). HOLDS — semantics are
  coherent: eligibility is evaluated live.
- **P3**: disabled worker's -1 → 403 at auth, no proposal stamped. HOLDS.
- **P4** (concurrent double -1 — the documented double-read race):
  the interleave resolved to quorum in-shim (second observer saw 2,
  pruned once); the fail-safe shape (both stamp, neither prunes, third
  -1 completes it) is the other legal outcome — either way ≤1 prune
  receipt and never a prune with <2 distinct witnesses. HOLDS.
- **P5** (raced prune batches): two handlers both observe quorum and
  both run the prune batch → exactly 1 prune receipt (conditional
  UPDATEs + `ON CONFLICT DO NOTHING`). A fourth -1 re-running the
  prune path → still 1. HOLDS.
- **P6**: same worker -1 on 3 of its own jobs → 200s, victim stays
  deferred, 0 prune receipts. One witness is one witness. HOLDS.

---

## HOLDS — completeClaimed validation + lifecycle races (Finding 2)

- **C1**: outcome type-confusion battery — `null`, missing key,
  `["SUCCEEDED"]`, `{x:1}`, `123`, `"SUCCEEDED "`, `""`,
  `"succeeded"`, `true` → all 400, claim stays `active`, claim still
  completable afterwards. (`Array.includes` is strict; JSON cannot
  smuggle a String object.) HOLDS.
- **C6** (disabled mid-claim): complete → 403; claim stays `active`;
  lease-expiry zombie sweep recovers it (`EXPIRED`, job deferred for
  replan) — no permanently stuck work. HOLDS.
- **C7** (two workers race `/v1/claim` for one lease): exactly one
  winner, exactly one claim row — the conditional-UPDATE atomic win
  holds under concurrency. HOLDS.
- **C8** (identical-body double-complete): first 200, replay 409 —
  the seal registry guards `/v1/complete` too. HOLDS.

---

## HOLDS — durable replay registry (Finding 3 re-attack)

- **S1**: seal re-ingested after TTL + cron prune → 200. The 1h
  replay window is documented and by design. HOLDS.
- **S2**: cross-isolate same-seal race ×10 rounds → exactly one 200 +
  one 409 every round **in-shim**. TRUE multi-writer D1 concurrency
  is not modeled (single-threaded shim) → D1 verdict UNKNOWN.
- **S3** (seal-flood economics, measured): 10,500 unique sealed
  ingests → 10,000 × 200 then fail-closed 503 (no evasion, no
  eviction); D1 `seal_registry` holds exactly 10,000 rows; the cron
  prune removes 0 fresh rows (bounded retention confirmed).
  Sustained-flood growth is capped at **10k rows per isolate per TTL
  window** because the `full` check precedes the D1 insert. Flood
  rate in-shim ≈ 44 ingests/s (10k in ~198s). Note: flooding requires
  *validly-signed* envelopes, so this vector is only reachable with
  the envelope authority key — at which point the threat model has
  bigger problems. HOLDS.
- **S4**: seal seen on isolate A → 409 on isolate B (D1 arbiter) and
  409 on A again (memory fast path). HOLDS.

---

## HOLDS — envelope ingest fuzzing

- **F1**: empty / truncated / `null` / array / string / number /
  binary-garbage / duplicate-key bodies → 400, no 500, no hang. HOLDS.
- **F2**: envelope missing/null/string/array/number → 400
  missing-envelope; `{}` (present-but-invalid) → 401 per the
  documented mapping. HOLDS.
- **F3**: tampered payload, tampered seal, bad sig, missing sig,
  numeric sig, wrong alg (`NONE:`), wrong env version, bad status,
  numeric seal → all 401. Authenticity-first holds; no bypass. HOLDS.
- **F4**: emoji keys/values + U+2028/29 → 200 (canonical UTF-8 byte
  sort consistent between minter and verifier); lone-surrogate body
  → 400, no 500. HOLDS.
- **F5**: 5MB validly-signed envelope → 200 (~0.5–2s, linear verify
  cost, no crash); 5MB *unsigned* garbage → 401 in ~70ms
  (fail-closed, linear cost). No explicit size cap exists, but cost
  is linear, not amplified. HOLDS (robustness).
- **F6**: 400-deep nested payload → 200, no stack blowup. HOLDS.
- **F7**: envelope used on `/v1/observe` replayed on `/v1/complete`
  → 409. The registry is route-agnostic. HOLDS.

---

## UNKNOWN — explicitly shim-limited or blocked

1. **Live-D1 `ON CONFLICT DO NOTHING` under true multi-isolate
   concurrency** (receipt unknown #1): the shim serializes SQLite
   writes in one thread. S2/P4/P5 prove the *logic*; whether two D1
   writers racing the same seal/prune behave identically is UNKNOWN —
   needs production traffic or a D1 load test ($0 constraint).
2. **Prune-path double-read race under real concurrency** (receipt
   unknown #2): demonstrated as a real interleave shape in-shim
   (fail-safe by design); production manifestation UNKNOWN, same
   constraint as (1).
3. **Historical batteries v3/v4/v5 + redteam2** (receipt unknown #5):
   NOT re-run — they target the live deployment
   (`lucidota-exchange.cutecity.workers.dev`) over external HTTPS,
   which this campaign forbids; the live worker also still runs the
   *pre-hardening* bundle, so they couldn't test v0.6.16 anyway.
   Their key vectors were re-implemented locally instead: v4-R1
   double-complete → C2/C8; v4-R2 claim race → C7.
4. **Deployed bundle** (receipt unknown #3): out of scope — no
   deploys performed per the hardening-only order.

### What the D1 shim can vs cannot prove

CAN: request-routing logic, validation ordering, SQL semantics of the
single-writer statements (conditional UPDATEs as race arbiters,
`ON CONFLICT DO NOTHING` idempotency), auth gating, envelope
cryptography, state-machine transitions, fail-closed mappings.
CANNOT: true multi-writer concurrency on D1 (all shim writes
serialize), D1 `db.batch()` atomicity under contention (the shim's
batch is sequential awaits — interleavable, unlike real D1),
production timing/latency windows, cold-start isolate behavior on
workerd (partially — the migrate race *was* caught because it is a
pure logic race, but its production hit-rate will differ).

---

## Breaks ranked by exploitability

1. **C3 sweep-interleaved double-execution** — integrity
   (double receipts/accounting). Demonstrated deterministically.
   *Not* reachable via standard lease paths today (leases ≤10min <
   15min stuck threshold) → latent. Becomes live the moment any
   >15min lease path exists. Fix cost: trivial.
2. **P7 migrate() cold-start race** — availability (transient 500s,
   75% of a 4-burst in-shim). Reachable on every schema-change
   deploy under burst. Self-healing, no corruption. Fix cost:
   trivial (promise guard or duplicate-tolerant `ensure()`).

## Files

- `tests/torture-prune.mjs` — P1–P7 (11 pass, P7 BROKEN)
- `tests/torture-complete.mjs` — C1–C8 (8 pass, C3 BROKEN)
- `tests/torture-seal.mjs` — S1–S4 (5 pass)
- `tests/torture-envelope.mjs` — F1–F7 (10 pass)
- `tests/torture-REPORT.md` — this file
