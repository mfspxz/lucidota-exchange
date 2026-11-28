# Exchange Test-Harness Isolation — Flake Fix (2026-11-22)

**Verdict: FIXED. Acceptance met — full workspace suite passes at DEFAULT
parallelism (no `--test-threads` flag) in 3 consecutive runs, 100 passed /
0 failed each, cargo exit 0 each.**

## Root cause (confirmed in source, matching the 2026-11-22 characterization)

`exchange-core/src/routes.rs` held the in-memory replay seal set in a
process-global static:

```rust
static SEEN_SEALS: LazyLock<Mutex<HashMap<String, i64>>>
```

`check_replay_db` consults this set before the durable `seal_registry`
table; a hit returns "replay" → HTTP 409. Every test in a binary shared
that one set, while the per-test databases were already isolated
(in-memory SQLite per test via `tests/harness.rs::fresh_db`).

Two coupling vectors:

1. **Identical seals across parallel tests.** `mint_envelope(claim_id)` in
   torture_complete / torture_prune builds a fixed payload
   (`{kind: completion[, worker_outcome: SUCCEEDED]}`), actor `luci`,
   prev `GENESIS`, timestamped at millisecond precision. Two tests
   minting for the same claim id in the same millisecond produce
   byte-identical envelopes → identical seals → whichever test's request
   landed second got a false 409 replay from the shared set. This is
   exactly the recorded c4 failure signature (`left: 409, right: 200` at
   torture_complete.rs:299). Which test lost the race varied run to run
   (p1, c4, c1 all observed) — the wandering failure identity.
2. **Cross-test clearing.** torture_seal's S1 simulated a fresh isolate
   via `clear_seen_seals_for_test()`, which wiped the GLOBAL set —
   including seals other parallel tests had just recorded.

`REPLAY_MAX_SEEN` (10,000) exhaustion was examined and ruled out as a
practical trigger (per-binary seal counts are in the tens). The
`SCHEMA_OK` atomic in migrate.rs was also examined: benign for tests —
`fresh_db()` runs `migrate()` directly per test DB, so the
`ensure_migrated` fast-path skipping later checks changes nothing.
Left untouched (it is deliberate production D1-burn protection).

## The fix — registry isolation via Env injection (harness, not assertions)

The seal set is now injected through `Env`, the existing host-abstraction
struct every route already receives:

- **`exchange-core/src/env.rs`** — new field
  `Env::seen_seals: Option<Arc<Mutex<HashMap<String, i64>>>>`.
  `None` = use the process global (production). `Some` = use the
  injected set (test-only isolation path; production code paths never
  set it).
- **`exchange-core/src/routes.rs`** —
  - `SEEN_SEALS` re-wrapped as `LazyLock<Arc<Mutex<…>>>` (same map, same
    lifetime; Arc so both paths share one type).
  - New `seen_seals(env)` resolver: Env-injected set if present, else
    the global.
  - `sweep_seen_seals` and `check_replay_db` now operate on the resolved
    set (`check_replay_db` gained the `env` parameter it already had
    one call away in `require_envelope`). Logic, ordering, TTL
    (1h), cap (10,000), and status mapping (409/503) are byte-for-byte
    the old behavior.
  - `clear_seen_seals_for_test(env: &Env)` now clears the set the given
    Env resolves to — a test can only clear its own registry.
- **`exchange-core/tests/harness.rs`** — `test_env()` injects a fresh
  empty seal set per call, so every test (and every test-built Env) is
  isolated by construction. All existing tests pick this up unchanged.
- **`exchange-core/tests/torture_seal.rs`** — S1's fresh-isolate
  simulation now passes its own `&env` to the clear helper. Same
  assertion, correctly scoped clear.

### ⚠️ Production-crate change (flagged per brief — the only one)

- **`exchange-worker/src/lib.rs`** — `build_env`'s `Env` struct literal
  gained `seen_seals: None` (with a comment saying why). This is the
  minimum change the new field forces on the production construction
  site, and it selects the pre-existing behavior: `None` → the
  per-isolate global, which on Workers is per-isolate by platform
  construction (own statics per WASM instance). **Production behavior
  is unchanged.** No other production source was modified. No worker
  artifact was rebuilt or redeployed; no wrangler file and no
  worker.js was touched (deployed `build/` artifact mtimes predate
  this session).

## Wall-clock hardening (deterministic margins)

- **Seal uniqueness no longer rides on the millisecond clock.**
  `mint_envelope` in torture_complete.rs and torture_prune.rs now adds
  a per-mint `nonce` payload field from an atomic counter — the exact
  pattern torture_seal.rs already used. Sequential re-mints inside one
  test (C1's nine-case battery, C2's "fresh envelope" second attempt)
  can no longer collide even if they land in the same millisecond.
  Payload extras are ignored by the handlers (binding is the top-level
  `claim_id`); no assertion was added, removed, or altered.
- Examined and deliberately left alone: the lease/expiry margins in the
  seeds are already deterministic (expiry = now + 1h; sweep vectors use
  `datetime('now','-20 minutes')` against a 15-minute threshold — a
  5-minute margin no scheduler delay on this box approaches), and C2's
  50 ms ordering sleep is a bias whose assertions are race-tolerant by
  design ([200,200] or [200,409], exactly 1 receipt + 1 accounting row).

## Acceptance runs

Command (each run): `cargo test --release --offline -j 1 --no-fail-fast`,
`TMPDIR=/home/hatch/workspace/.tmp-exchange-test`, **default test
parallelism — no `--test-threads` flag**. RAM gate (≥800 MB available)
checked before every run.

| Run | RAM avail. before | Result | Cargo exit |
|---|---|---|---|
| 1 | 2,738 MB | 100 passed / 0 failed (17 suites ok) | 0 |
| 2 | 1,743 MB | 100 passed / 0 failed | 0 |
| 3 | 1,647 MB | 100 passed / 0 failed | 0 |

The 2 ignored tests are the pre-existing live-only `creds_live` pair
(ignored before this change, by design). The historically flaky tests —
`c4_sweep_leaves_fresh_completion`, `c1_outcome_type_confusion`,
`p1_sequential_quorum_no_double_prune`, plus seal suites
`s1_ttl_readmission_by_design` and `s2_concurrent_same_seal_race` —
passed in all three runs. Logs: `~/workspace/.tmp-exchange-test/harness-run{1,2,3}.log`.

## Scope guarantees

- No assertion weakened, no test deleted or ignored, no production
  behavior change (see the flagged one-field literal above).
- No git commits, no wsign, no artifact rebuild/redeploy, no wrangler
  or worker.js changes.

## Files changed

- `rs/exchange-core/src/env.rs` — `Env::seen_seals` field (+ docs).
- `rs/exchange-core/src/routes.rs` — Arc-wrapped global, `seen_seals(env)`
  resolver, parameterized sweep/check, Env-scoped clear helper.
- `rs/exchange-worker/src/lib.rs` — `seen_seals: None` in `build_env` (flagged above).
- `rs/exchange-core/tests/harness.rs` — per-Env fresh seal set injection.
- `rs/exchange-core/tests/torture_seal.rs` — S1 clears its own Env's set.
- `rs/exchange-core/tests/torture_complete.rs`, `rs/exchange-core/tests/torture_prune.rs` — nonce-unique mint helpers.
