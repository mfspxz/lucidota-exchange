# Exchange Rust Port — Gaps Closed (2026-11-22)

Closes the 8 gap-closing steps from `FLIP-READINESS-2026-11-22.md` §6
against `~/workspace/exchange/rs/`. The Rust tree is now a port of
worker **v0.7.3** (was v0.6.16). **No deploy was performed.** Production
`worker.js` (mtime 2026-11-21) and the root `wrangler.toml`
(main = "worker.js") were not touched — verified after the work.

Method: worker.js read as the reference implementation (routes,
govern(), claim, completeClaimed, migrate); SQL strings copied
verbatim; behavior locked in by a new parity suite,
`rs/exchange-core/tests/v07_parity.rs` (**21 tests, all passing**).
All builds `-j 1 --offline` with a free-RAM check before each cargo
invocation (box: 7.9 GB total, 0.7–2.6 GB available during the work).

---

## Gap 1 — `/v1/capabilities` + `/v1/config` + `/v1/config/log` routes, same D1 tables

**Implemented.**

- `rs/exchange-core/src/migrate.rs`: v0.7.0 DDL added to `migrate()` —
  `model_capabilities` (PK (resource_id, model)) + `idx_model_caps_model`
  + `idx_model_caps_capability`, `key_config`, `config_log`,
  `exchange_config`, and the fail-closed seed
  `INSERT OR IGNORE INTO exchange_config (key,value,updated_at) VALUES ('allow_paid','0',?)`.
  Byte-identical to worker.js migrate() (worker.js:640–658). A fresh D1
  or reset now recreates the full v0.7 schema; against the live D1 all
  statements are IF NOT EXISTS / OR IGNORE (additive, no-ops).
- `rs/exchange-core/src/routes.rs`: four new handlers behind the same
  Exchange-key gate, inserted at the same point in the route chain as
  the JS (after `/v1/capacity`, before ballots):
  - `capabilities()` — the JS JOIN over `model_capabilities` +
    `resources`, `key_id` derived from resource config, `key_enabled`
    from key_config, `usable` = evidence usable ∧ state ≠ QUARANTINED ∧
    key enabled ∧ cost ≠ unfunded, `proven` = last_executed_at set;
    filters `?key=`, `?capability=`, `?cost=`, `?usable=1`;
    `by_capability` / `by_cost` counted post-filter, exactly like the JS.
  - `get_config()` — keys ordered by failover_order, key_id; allowlist
    JSON-parsed (null on parse failure); `enabled` as boolean; global
    map from exchange_config; `recent_changes` = last 20 config_log rows.
  - `get_config_log()` — last 100 rows, `?key_id=` filter.
  - `post_config()` — global scope: only `allow_paid`, value 0/1/"0"/"1",
    INSERT OR REPLACE + config_log + evidence_log rows, response
    includes the PAID EXECUTION ENABLED note. Key scope: key must be
    backed by a registered resource (else 400 `unknown key_id …`);
    fields `enabled` / `allowlist` / `allowlist_clear` / `cost_cap` /
    `failover_order` with the JS validation errors verbatim; allowlist
    entries validated against listed models
    (`allowlist models not listed for any resource: …`); every change
    written to key_config (INSERT OR REPLACE), receipted in config_log
    and mirrored into evidence_log (`config:key:<id>`), response carries
    `log_id`. One implementation note: the JS reads
    `meta.last_row_id`; the Rust `Db` trait exposes no last-row-id, so
    the insert uses `INSERT … RETURNING id` — same value, single
    statement.
- Tests: `migrate_creates_v07_tables_and_allow_paid_seed`,
  `capabilities_listing_counts_and_filters` (totals usable=2/proven=1,
  by_capability/by_cost, all four filters, disabled-key → usable=0,
  401 without the admin key), `config_global_roundtrip_and_validation`,
  `config_key_roundtrip_and_validation` (incl. config/log key filter
  and the evidence_log mirror).

## Gap 2 — Config enforcement in the claim/govern path

**Implemented.** New module `rs/exchange-core/src/config.rs`:
`load_key_configs()` (SELECT * FROM key_config; any DB error →
empty map, the JS pre-migration try/catch semantics) and
`load_allow_paid()` (true only for the exact value "1"; error → false,
fail-closed).

`rs/exchange-core/src/govern.rs` `govern()` now loads both once per
call (after the EVIDENCE gate, as the JS does) and runs the JS
MODEL + CONFIG block per candidate for inference/embedding jobs,
in the JS order, with the JS rejection strings and taxonomies:

- key disabled → CONFIG / POLICY_FAILURE (`key <id> disabled by config`)
- no model bound → MODEL / CAPABILITY_FAILURE
- pinned model + `model_exact=false` → MODEL / CAPABILITY_FAILURE
  (failover pool refusal)
- effective model ∉ key allowlist → CONFIG / POLICY_FAILURE
- model row capability ≠ job capability → MODEL / CAPABILITY_FAILURE
- model row cost `unfunded` → COST / COST_FAILURE
- model row cost `paid`: allow_paid=0 → COST_FAILURE; no spend
  authority → COST_FAILURE; spend_authority > key cost_cap →
  CONFIG / POLICY_FAILURE
- no model row: resource HAS rows → MODEL refusal (`not listed for`);
  resource has NO rows (legacy) → config.model binding governs
  (mismatch with a pinned model refuses)

GATE_ORDER gains MODEL and CONFIG (vacuous-gate recording matches the
JS list). EFFICIENCY survivors carry the effective model; scored /
ranked / selected objects now include `model`, and score ties break
by key_config `failover_order` (lower first) — an ordering input only,
never an authorization, as in the JS.

Tests (one fresh DB each): `govern_baseline_selected_with_model`,
`govern_disabled_key_refuses`, `govern_allowlist_refuses`,
`govern_unlisted_model_refuses`, `govern_model_capability_mismatch_refuses`,
`govern_unfunded_refuses`, `govern_paid_model_kill_switch_and_cost_cap`
(exercises the full chain: kill-switch refusal → knob flipped through
the real POST /v1/config route → no-spend-authority refusal → cost_cap
refusal → under-cap job routes), `govern_failover_pool_refuses_pin`.

## Gap 3 — Claim-time exact-model binding + MODEL_MISMATCH taxonomy

**Implemented.**

- Delegation (routes.rs `/v1/claim`): `requested_model` is now the
  JOB's pinned model first (`spec.input.model`), the resource config
  default only when the job pins none; `model_exact`
  (`config.model_exact !== false`) added to the delegation. This
  replaces the v0.6.16 port's config-only `requested_model`.
- Completion (claims.rs `complete_claimed`): requested model resolved
  the same way (job first). On model_match=0 the JS evidence_log entry
  is now joined by the model_capabilities `last_error` stamp
  (`model-mismatch: requested X, executed Y`). Verification: a
  SUCCEEDED completion with model_match=0 on an exact resource fails
  verification with the JS note text and `error_class` **MODEL_MISMATCH**
  (failClass precedence: mismatch → MODEL_MISMATCH, other verification
  failures → VERIFICATION_FAILED, else the worker's class) in both the
  execution row and the receipt fallback chain. Failover-pool
  resources (`model_exact=false`) are exempt, as in the JS.
- Taxonomy (policy.rs `taxonomy_of`): `NOT_FOUND` / `BAD_REQUEST` →
  PERMANENT_FAILURE, `MODEL_MISMATCH` → VERIFICATION_FAILURE —
  the v0.7.3 strings/classes, unit-tested in policy.rs and again in
  `taxonomy_v073_classes`.
- Tests: `claim_binds_job_model_and_resume_returns_same_claim`
  (delegation asserts), `claim_delegation_model_exact_false_pool`,
  `complete_model_mismatch_is_verification_failure` (job state
  `failed` — not deferred; execution row carries
  MODEL_MISMATCH / VERIFICATION_FAILURE / model_match=0),
  `complete_mismatch_exempt_for_failover_pool`,
  `complete_not_found_is_permanent_and_demotes_default` (taxonomy
  consequence end-to-end).

## Gap 4 — Claim `resume:true` (v0.7.2)

**Implemented** in routes.rs `/v1/claim`, before the fresh-claim path,
with the JS query verbatim (caller's active claims joined to jobs,
leases in state 'claimed', lease_expiry in the future, latest-expiring
first). Returns `{claimed:true, resumed:true, delegation:{…same shape
as a fresh delegation, incl. requested_model/model_exact…}, lease_id,
job_id, attempt_id}`. No live claim of the caller's → falls through to
a fresh claim. Another worker's resume finds nothing (verified in
test: claimed:false, no theft, no new orphan).

Test: `claim_binds_job_model_and_resume_returns_same_claim` (same
claim_id + attempt_id returned; cross-worker resume refused;
resume-with-no-claim falls through and claims fresh).

## Gap 5 — Inference-first ordering in the verify gauntlet (v0.7.1)

**Implemented** in routes.rs `verify_resource()`: the capability
probe selection is now
`SELECT capability FROM resource_capabilities WHERE resource_id=? ORDER BY CASE WHEN capability='inference' THEN 0 ELSE 1 END`
— verbatim from worker.js:2745. Test:
`verify_gauntlet_prefers_inference` inserts embedding FIRST (the
insertion order that triggered the JS bug) and asserts the delegated
verification job is created with capability `inference`.

## Gap 6 — model_capabilities execution bookkeeping on complete

**Implemented** in claims.rs, at the JS position (after the execution
and lease updates):

- success with an actual model →
  `UPDATE model_capabilities SET last_executed_at=?, last_error=NULL WHERE resource_id=? AND model=?`
- worker-reported NOT_FOUND for the requested model →
  `last_error = 'model unavailable at provider (404/410) — claimant refused without executing'`,
  a `model-unavailable` evidence_log row, and — when the dead model
  was the resource's config default — `set_evidence(UNKNOWN,
  force=true)` demotion with the JS detail text.
- (The match=0 `last_error` stamp is listed under Gap 3; it is the
  third bookkeeping write, also at its JS position.)

Tests: `complete_success_stamps_model_capabilities` (last_executed_at
set, last_error cleared), `complete_not_found_is_permanent_and_demotes_default`
(last_error text exact; evidence demoted VERIFIED → UNKNOWN; both
`model-unavailable` evidence rows present — the bookkeeping insert
and setEvidence's own log, matching the JS's two rows).

## Gap 7 — Wrangler packaging for the Rust worker

**Implemented and proven — the artifact exists.**

- `rs/exchange-worker/wrangler.toml` (new): name `lucidota-exchange`,
  `main = "build/worker/shim.mjs"`, compatibility_date `2026-10-01`,
  same account_id, `[build] command = "cargo install -q worker-build && worker-build --release"`,
  D1 binding `EXCHANGE_DB` → database_id
  `0ea89801-9b0a-4efa-9d2f-170d7d285e69` (identical to production),
  cron trigger `*/30 * * * *`. No KV. Secrets are not declared (set
  via `wrangler secret put`, as production): EXCHANGE_KEY, PL_SIG_KEY.
- Build proof: `worker-build v0.8.7` installed from the local cargo
  registry cache (`cargo install worker-build --offline`; the first
  attempt died because cargo-install builds under /tmp — the 512 MB
  tmpfs — and rustc was killed mid-crate; retry with TMPDIR +
  CARGO_BUILD_BUILD_DIR on real disk succeeded). Then
  `worker-build --release` in rs/exchange-worker produced:
  - `build/worker/shim.mjs` (the wrangler `main`)
  - `build/index.js` (esbuild bundle, 27.0 KB)
  - `build/index_bg.wasm` (wasm-opt optimized; build/ totals 1.7 MB)
- A staged deploy from this config is the flip procedure in
  FLIP-READINESS §7 — **not executed here** (no deploy, per orders).

## Gap 8 — Version stamp parity

**Implemented.** `exchange-core/src/lib.rs`: `VERSION = "0.7.3"`,
served by `/` and `/v1/health`. Crate versions bumped 0.6.16 → 0.7.3
in rs/exchange-core, rs/exchange-worker, rs/exchange-organelles
Cargo.toml. `/v1/health` also gained the JS's v0.7.0 counts —
`capabilities` (model_capabilities count) and `keys` (key_config
count), 0 when the tables are absent (JS try/catch parity).
Test: `version_stamp_and_health_counts`.

---

## Test results

New suite — `cargo test -p exchange-core --test v07_parity`:
**21 passed, 0 failed** (debug and release).

Full workspace — `cargo test --release --offline -j 1 --no-fail-fast`
(after `cargo build --release`; the release exchange-organelles
binary was copied to the `target/debug/` path that
exchange-organelles/tests/wire_compat.rs hardcodes — the same
disclosed harness shim the readiness report used):

| Suite | Result |
|---|---|
| exchange-core lib (unit) | 18 passed |
| harness | 0 (scaffolding) |
| torture_complete | 3 passed, **1 failed (flaky — see below)** |
| torture_envelope | 4 passed |
| torture_prune | 1 passed, **2 failed (flaky — see below)** |
| torture_seal | 3 passed |
| **v07_parity (new)** | **21 passed** |
| exchange-organelles lib | 36 passed |
| exchange-organelles main | 0 |
| creds_live | 0 passed, 2 ignored (live-only, correct) |
| organelles | 5 passed |
| socket | 2 passed |
| wire_compat | 3 passed (incl. Python-adapter wire parity) |
| exchange-worker lib | 0 tests |
| doc-tests | 0 |

96 passed in that run; the only failures are the two suites the
readiness report already flagged as flaky. First (fail-fast) run
aborted at torture_prune after torture_complete had passed 4/4 —
the flake moves between runs, which is its signature.

### The two known flaky torture tests — standalone characterization

Not hidden, not fixed (out of scope per orders; both predate this
work and are documented in FLIP-READINESS §2):

- `torture_prune::p1_sequential_quorum_no_double_prune` —
  full-binary runs: failed in 3 of 5 repetitions. Run **alone**
  (single-test filter): **6 of 6 passed**. p2/p6 passed in every run.
- `torture_complete::c4_sweep_leaves_fresh_completion` —
  **alone: 5 of 5 passed**; in full-binary runs it passed 5 of 5
  here (the readiness report saw it fail once in a full-workspace
  run with `left: 409, right: 200`). In one of my five full-binary
  repetitions the failure surfaced in a *different* test of the same
  suite (`c1_outcome_type_confusion`, 1 of 5 runs) — the suite's
  tests share a process-global envelope seal registry and race
  wall-clock sweep/lease semantics on this 1.5-CPU box; which test
  loses the race varies. A green full-workspace run on a quiet box
  remains a pre-flip condition (FLIP-READINESS §6.8), unchanged.

### WASM target re-check (required)

`cargo check --target wasm32-unknown-unknown -p exchange-worker --offline -j 1`
— **PASS** (exit 0), run after all changes. Plus the stronger proof
under Gap 7: worker-build compiled and packaged the release wasm.

## Files changed

- `rs/exchange-core/src/lib.rs` — VERSION 0.7.3; `pub mod config`
- `rs/exchange-core/src/config.rs` — **new**: key_config / allow_paid reads
- `rs/exchange-core/src/migrate.rs` — v0.7.0 tables + indexes + allow_paid seed
- `rs/exchange-core/src/policy.rs` — v0.7.3 taxonomy classes + unit asserts
- `rs/exchange-core/src/govern.rs` — MODEL + CONFIG gates, failover tie-break, model in scoring output
- `rs/exchange-core/src/claims.rs` — job-first requested model, MODEL_MISMATCH verification + failClass, model_capabilities bookkeeping (stamp / NOT_FOUND / mismatch)
- `rs/exchange-core/src/routes.rs` — health counts; claim resume + delegation binding; verify ORDER BY; /v1/capabilities, /v1/config (GET/POST), /v1/config/log
- `rs/exchange-core/tests/v07_parity.rs` — **new**: 21 parity tests
- `rs/exchange-core/Cargo.toml`, `rs/exchange-worker/Cargo.toml`, `rs/exchange-organelles/Cargo.toml` — version 0.7.3 (Cargo.lock regenerated by the builds)
- `rs/exchange-worker/wrangler.toml` — **new**: packaging config
- `rs/exchange-worker/build/` — **new**: worker-build output (shim.mjs, index.js, index_bg.wasm)

## Could NOT close / remaining notes

- Nothing in the 8-step list is unimplemented. Two honest caveats:
  1. **Readiness §6.8 (one clean full-workspace pass) is still not
     producible on this loaded box** — the two pre-existing flaky
     torture tests still flake under concurrency (characterized
     above; both pass standalone repeatedly). This is the same state
     the readiness report recorded; my changes neither fixed nor
     worsened it (failure identity moves run-to-run, including to a
     test, c1, that the readiness runs saw pass and vice versa).
  2. **Runtime parity against a live D1 is unproven by construction**
     — all tests run on the rusqlite harness (the repo's established
     parity method, incl. wire parity with the Python adapter). The
     staged-deploy comparison in FLIP-READINESS §7 steps 3–4 remains
     the live proof, and is a deploy action — out of scope here.
- Minor known divergence, inherited from the v0.6.16 port and left
  as-is: build_env's comment in exchange-worker/src/lib.rs overclaims
  provider-secret passthrough (code passes only PL_SIG_KEY +
  EXCHANGE_KEY); adapters read provider config from D1, so impact is
  nil, but the comment should not be trusted as behavior
  (FLIP-READINESS §5).
