# Exchange Rust Flip — Readiness Proof (2026-11-22)

**Verdict: NOT READY.**

The Rust replacement is real, compiles for the actual Workers target, and its
protocol core is test-backed. But it is a port of worker **v0.6.16** while
production runs **v0.7.3**. Flipping `wrangler.toml` today would silently
delete the v0.7.0 knobs/dials surface, stop enforcing config already written
to D1, drop claim-time exact-model binding, and reintroduce the claim-orphan
bug that v0.7.2 fixed. Every gap below carries file evidence.

Method: owner-run builds and tests on this box (2026-11-23 ~03:40–04:20 UTC),
source-level route/state comparison of `~/workspace/exchange/worker.js`
(3,479 lines, `VERSION = "0.7.3"`, worker.js:83) against
`~/workspace/exchange/rs/` (workspace: exchange-core + exchange-worker +
exchange-organelles, all version 0.6.16; `exchange-core/src/lib.rs:24` —
`pub const VERSION: &str = "0.6.16"`). No deploy was performed; wrangler.toml
was not touched.

---

## 1. Build proof

| Check | Result | Evidence |
|---|---|---|
| `cargo build --release --offline -j 1` (whole rs workspace) | **PASS**, exit 0 | Finished release in 16m05s (resumed run — the first attempt was interrupted by a runtime restart at the dependency stage, ~286 dep artifacts in; cargo resumed from fingerprints). 2 warnings only (`unused_mut` in exchange-worker/src/lib.rs:120). |
| `cargo check --target wasm32-unknown-unknown -p exchange-worker` | **PASS**, exit 0 | Finished in 3m37s. The deploy-target compile is proven, not assumed. |
| Deployable artifact packaging | **NOT WIRED** | `rs/exchange-worker/` contains only Cargo.toml + src/. There is no wrangler.toml, no worker-build config, no `build/worker/shim.mjs` + `.wasm` output anywhere under rs/. `cargo check` proves compilation; nothing has ever produced the packaged Workers artifact. |
| Test suite | **PASS with two flaky tests + one harness caveat** | Detail in §2. |

RAM at build start: 2,511 MB available (no RAM-BLOCKED condition hit).

## 2. Test results (cargo test --release --offline -j 1)

78 tests across 10 suites. Clean passes: exchange-core lib 18/18,
torture_envelope 4/4, torture_seal 3/3, exchange-organelles lib 36/36,
socket 2/2, harness 0 (scaffolding only), creds_live 2 ignored (live-only,
correctly ignored), exchange-worker 0 tests.

- **Harness-path failures (not code failures):** `organelles.rs::adapter_smoke_suspend`
  and `wire_compat::{bad_json_parity, wire_parity_with_python_adapter}` spawn
  `target/debug/exchange-organelles` (hardcoded in
  exchange-organelles/tests/wire_compat.rs, `rust_bin()`), which did not exist
  because only `--release` was built. After copying the release binary to that
  path, all 8 tests pass — including **full wire parity with the Python
  adapter and receipt-id parity** (wire_compat 3/3, organelles 5/5). The copy
  is disclosed here; the tests exercise identical source in release profile.
- **Flaky under load (readiness-relevant):** two full-workspace
  `cargo test` runs each exited 101 on a *different* timing-sensitive test:
  - `torture_prune::p1_sequential_quorum_no_double_prune` — failed in full
    run 1 and in 2 of 3 paired re-runs; passes standalone repeatedly.
  - `torture_complete::c4_sweep_leaves_fresh_completion` — failed in full
    run 2: assertion at torture_complete.rs:299, `left: 409, right: 200`
    (complete-after-sweep returned 409 instead of 200); passed in 4 other runs.
  Both race sweep/lease-expiry wall-clock semantics on a shared 1.5-CPU box.
  Not proof of a code bug — but the suite currently **cannot produce a clean
  full-workspace pass**, and sweep/prune is exactly the machinery a flip
  must trust. A green full run on a quiet box is a pre-flip condition.

## 3. Route parity

JS serves 21 exact-path route entries + 5 regex (dynamic) routes.
Rust serves 17 + 5. **Missing in Rust: the entire v0.7.0 config/capabilities
surface.**

| JS route (worker.js) | In Rust? | Notes |
|---|---|---|
| GET `/` | ✅ routes.rs:316 | Reports version "0.6.16" vs JS "0.7.3" |
| GET `/v1/health` | ✅ routes.rs:322 | JS also returns `capabilities` (model_capabilities count) + `keys` (key_config count), worker.js:2511–2522; Rust returns neither |
| POST `/v1/workers/register` | ✅ routes.rs:332 | Worker-scoped auth present both sides |
| POST `/v1/claim` | ⚠️ routes.rs:380 | **Behavior gaps — see §4** |
| POST `/v1/complete` | ⚠️ routes.rs:506 | Envelope + completion present; **taxonomy/model gaps — see §4** |
| POST `/v1/admin/sweep-expired-claims` | ✅ routes.rs:522 | |
| POST `/v1/seed` | ✅ routes.rs:535 | |
| GET/POST `/v1/resources` | ✅ routes.rs:541/545 | |
| POST `/v1/discover` | ✅ routes.rs:559 | |
| GET `/v1/resources/:id/evidence` | ✅ routes.rs:574 | regex in JS at worker.js:2716 |
| POST `/v1/resources/:id/verify` | ⚠️ routes.rs:602 | JS v0.7.1 picks the probe capability inference-first (`ORDER BY CASE WHEN capability='inference'…`, worker.js:2745); the Rust SELECT (routes.rs:774) has no such ordering — the embedding-first gauntlet bug v0.7.1 fixed is still latent in Rust |
| GET `/v1/capacity` | ✅ routes.rs:608 | |
| GET `/v1/capabilities` | ❌ **ABSENT** | JS worker.js:2880–2922. Zero references to `model_capabilities` anywhere in rs/ (grep: 0 files). The route every claimant uses to enumerate 656 capabilities / per-model routing does not exist in Rust |
| GET `/v1/config` | ❌ **ABSENT** | JS worker.js:2923. Zero references to `key_config` / `exchange_config` in rs/ |
| GET `/v1/config/log` | ❌ **ABSENT** | JS worker.js:2938. Zero references to `config_log` in rs/ |
| POST `/v1/config` | ❌ **ABSENT** | JS worker.js:2946–3110: per-key `enabled` / `allowlist` / `cost_cap` / `failover_order` + global `allow_paid`, every change receipted into config_log + receipts. None of it exists in Rust |
| POST `/v1/ballots` | ✅ routes.rs:613 | Envelope-gated both sides |
| GET `/v1/ballots/:id` | ✅ routes.rs:623 | JS regex worker.js:3180 |
| GET `/v1/ballot-policy` | ✅ routes.rs:636 | |
| POST `/v1/job` | ✅ routes.rs:646 | |
| GET `/v1/job/:id` | ✅ routes.rs:655 | JS regex worker.js:3200 |
| GET `/v1/jobs` | ✅ routes.rs:679 | |
| GET `/v1/receipts` | ✅ routes.rs:697 | |
| GET `/v1/receipts/:id/verify` | ✅ routes.rs:712 | JS regex worker.js:3231 |
| POST `/v1/observe` | ✅ routes.rs:717 | Envelope-gated both sides |
| `scheduled()` cron (*/30) | ⚠️ lib.rs:195 + routes.rs:1323 | Rust has the v0.6.15-era reaction loop (migrate, memoized promote/demote, reaper). Structurally parallel; it maintains no v0.7 state because the v0.7 features don't exist on the Rust side |

Envelope verification **is** at parity: Rust `require_envelope`
(routes.rs:236) mirrors JS `requireEnvelope` (worker.js:287) — same
fail-closed 503 when the authority key is unprovisioned, same
seal_registry replay arbitration, same claim binding, applied to
complete / ballots / observe on both sides.

## 4. Behavior gaps on routes that exist

1. **Exact-model binding (v0.7.0) — claim time missing.** JS delegation
   carries `requested_model: (spec.input && spec.input.model) || rconf.model`
   and `model_exact: rconf.model_exact !== false` (worker.js:2658–2662), and
   claim routing refuses models not listed in `model_capabilities` for the
   resource (worker.js:1727, 1884–1947). Rust: `model_exact` appears **nowhere**
   in rs/; the claim path never consults a per-model listing (there is no
   `model_capabilities` table access at all). Rust tracks requested-vs-actual
   only at *complete* time as an audit trail (claims.rs:368–385, W9
   model_match + evidence_log) — detection after the fact, not the binding
   refusal Zack ordered 2026-11-20 ("requested model executes exactly or the
   job refuses/fails — no silent substitution, ever").
2. **MODEL_MISMATCH taxonomy (v0.7.3) missing.** JS `taxonomyOf` maps
   MODEL_MISMATCH → VERIFICATION_FAILURE and NOT_FOUND/BAD_REQUEST →
   PERMANENT_FAILURE (worker.js:895–897). No such classes exist in Rust
   (grep `MODEL_MISMATCH`: 0 hits in rs/).
3. **Execution bookkeeping on model_capabilities missing.** JS stamps
   `last_executed_at` / `last_error` per (resource, model) on completion
   (worker.js:1129, 1208, 1212) — the data `/v1/capabilities`' `proven` flag
   and the Nov 23 health counts (42 resources / 656 capabilities) are read
   from. Rust never writes these.
4. **Claim resume (v0.7.2) missing.** JS `/v1/claim` with `resume:true`
   returns the caller's live unexpired delegation instead of minting a new
   lease (fixes the truncated-response claim orphans of 2026-11-21).
   `resume` appears nowhere in Rust claims.rs. A flip reintroduces the
   orphan bug.
5. **Config enforcement missing (the operational landmine).** Since
   2026-11-20, operators have written real settings through `/v1/config`
   into `key_config` / `exchange_config` in the live D1. Rust never reads
   those tables. After a flip, a key an operator disabled would serve
   again, allowlists and cost caps would stop applying, and
   `allow_paid` would revert to whatever the Rust code assumes — silently,
   with no error anywhere.

## 5. State / bindings parity

| Item | JS (deployed) | Rust | Match? |
|---|---|---|---|
| D1 binding | `EXCHANGE_DB` → 0ea89801-9b0a-4efa-9d2f-170d7d285e69 (wrangler.toml) | `env.d1("EXCHANGE_DB")` (exchange-worker/src/lib.rs:181, 197) | ✅ same name |
| Cron | `*/30 * * * *` (wrangler.toml triggers) | `#[event(scheduled)]` handler present (lib.rs:195) | ✅ handler exists; trigger lives in the (not-yet-written) Rust wrangler config |
| Secret `EXCHANGE_KEY` | env.EXCHANGE_KEY, fail-closed 503 if missing | env.secret("EXCHANGE_KEY") (lib.rs:143), same fail-closed semantics in core | ✅ |
| Secret `PL_SIG_KEY` | env.PL_SIG_KEY (envelope authority) | passed through in build_env (lib.rs:135) | ✅ |
| KV | none | none | ✅ |
| Provider secrets | Live in D1 resource config on both sides | build_env comment claims prefix passthrough of provider secrets, but the code inserts only PL_SIG_KEY + EXCHANGE_KEY (lib.rs:134–144) — comment overclaims; practical impact low because adapters read D1 config, but the comment should not be trusted as behavior | ⚠️ comment/code mismatch |
| D1 schema ownership | JS migrate() creates 16 tables incl. `key_config`, `exchange_config`, `config_log`, `model_capabilities`, `receipts` + idx_model_caps_* + idx_leases_state_created | Rust migrate.rs creates the v0.6.16 subset (10 CREATE TABLEs captured; missing those 5 tables and those indexes) | ⚠️ Safe against the *existing* D1 (all CREATEs are IF NOT EXISTS, additive, nothing dropped), but a fresh D1 or reset would silently lose the v0.7 schema, and Rust boot would not recreate it |

## 6. What "ready" requires (gap list, in order)

1. Port `/v1/capabilities` + `model_capabilities` (schema, enumeration
   write paths, claim-time listing checks) to exchange-core.
2. Port `/v1/config` GET/POST + `/v1/config/log` with `key_config`,
   `exchange_config`, `config_log` — including **enforcement** in the
   claim path (enabled / allowlist / cost_cap / failover_order /
   allow_paid), not just the routes.
3. Port claim-time exact-model binding (delegation `requested_model` from
   the job spec first, `model_exact` flag, refusal semantics) +
   MODEL_MISMATCH / NOT_FOUND / BAD_REQUEST taxonomy classes +
   model_capabilities execution bookkeeping on complete.
4. Port claim `resume:true` (v0.7.2).
5. Port the v0.7.1 inference-first capability ordering in the verify
   gauntlet.
6. Health/root version parity (report capabilities + keys counts; version
   string must stop saying 0.6.16).
7. Wire the deploy packaging: wrangler config for exchange-worker
   (worker-build), D1 binding + */30 cron trigger carried over, and a
   deploy script path equivalent to deploy-exchange-070.py for the wasm
   artifact.
8. Get one clean full-workspace `cargo test` pass (the two flaky
   sweep/prune tests green in the same run) on a quiet box.

## 7. Flip procedure (for when §6 closes — NOT executed)

1. Backup: copy deployed worker.js + wrangler.toml into
   `~/workspace/exchange/backups/` (existing pattern); snapshot D1
   (d1-mirror refresh) and record current `/v1/health` output
   (resources / capabilities / keys counts) as the parity baseline.
2. Build the wasm artifact via worker-build from rs/; verify the artifact
   size + that the build embeds exchange-worker.
3. Stage: deploy the Rust worker to a scratch worker name on the **same**
   D1 binding; run the repo's worker batteries (tests/ — worker-battery
   v4 integrity + claim/complete flows) against the staged URL, including
   a pinned-model claim, a config read/write round-trip, and an envelope
   complete.
4. Compare staged `/v1/health` + `/v1/capabilities` output against the
   baseline from step 1 — counts must match.
5. Flip: point the production wrangler config's build at the Rust
   artifact (keep name, D1 binding, cron, secrets unchanged) and deploy.
6. Monitor: `/v1/health`, claim/complete success rate, and the D1
   read-burn rate for one full cron cycle (*/30) before declaring done.
7. **Rollback:** restore the backed-up wrangler.toml (main = worker.js)
   and redeploy worker.js via the existing deploy path
   (deploy-exchange-070.py). No D1 rollback needed — all schema changes
   are additive and the JS worker tolerates the Rust era's rows; config
   written during the Rust window must be re-checked in `/v1/config/log`
   after rollback.

---

*Proof standard: every claim above is tied to a command run or a file:line
read during this audit. Where a check could not distinguish (flaky tests),
the report says so instead of rounding up.*
