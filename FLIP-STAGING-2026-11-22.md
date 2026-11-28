# Exchange Flip — Staging Report (FLIP-READINESS §7, steps 1–4)

Executed 2026-11-23 ~05:15–05:35 UTC. Step 5 (the production flip) was
NOT executed — it is the owner's step.

## VERDICT: DO NOT FLIP — one hard parity failure

The staged Rust worker **rejects every valid lucidota-receipt/1 envelope**
with `401 envelope: sig mismatch — envelope NOT sealed by the authority
key`, including envelopes sealed with the exact key provisioned on the
staging worker itself. Production (worker.js v0.7.3) accepts the same
envelopes. If flipped as-is, every envelope-gated route
(`/v1/complete`, `/v1/ballots`, `/v1/observe`) would refuse all
legitimate claimant traffic and the exchange would stop accepting
completions. Everything else tested is at parity (details below).

Production is 100% untouched: no deploy, no config change, root
`wrangler.toml` unmodified; `worker.js` / `wrangler.toml` sha256 at the
end of the run are identical to the step-1 backup, and production
`/v1/health` still reports v0.7.3 / 42 / 656 / 11.

---

## Step 1 — Backup + baseline ✅

- `~/workspace/exchange/backups/2026-11-22-flip-staging/`:
  - `worker.js` — 220,797 B, sha256 `76b8292854b2ae84402d128524ea6e6a53314b7bd04453474f80d2c5f7c3dcd3`
  - `wrangler.toml` — 533 B, sha256 `f8a2f13b2c2ba112ac3bea65469ce1f1ff57a78d41035c96d4988f6603827aa5`
  - `PARITY-BASELINE.json` — production `/v1/health` at 2026-11-23T05:15:28Z:
    version **0.7.3**, resources **42**, capabilities **656**, keys **11**.
    `/v1/capabilities`: total **656**, entries **656**, proven **9**,
    usable **0**, by_capability {inference 589, embedding 25, other 42},
    by_cost {free 102, paid 550, unfunded 4}. (Values cross-confirmed via
    two transports; the VM→Worker path truncates large responses via
    urllib — known F10 fault — curl reads succeeded.)
- D1 snapshot: `refresh-mirror.py` run to completion —
  **40,023 rows, 23 tables, drift=0, mirror integrity ok**
  (`d1-mirror.db` refreshed 2026-11-23T05:18Z; its `.sig` is now stale,
  re-signing is the owner's routine).

## Step 2 — Artifact ✅ (twice: original, then A3-patched rebuild)

- Original artifact was current (no rs/ source file newer than
  `build/index_bg.wasm`): wasm 1,727,321 B sha256 `f6b72899…`, index.js
  27,675 B sha256 `0c3c24f0…`.
- After the A3 patch (below) the artifact was rebuilt with
  `worker-build --release`: wasm 1,732,066 B sha256
  `9cf5f375dad905b10418d295f2634324c0767b95b48d71045deece256ce7cc54`;
  index.js unchanged.

## A3 D1-burn patch — applied mid-run on the A3 lane's order ✅

- `D1-BURN-FIX-A3.rust.patch` applied clean (`patch -p0`) to
  `exchange-core/src/migrate.rs` + `routes.rs`: `SCHEMA_EPOCH =
  "v0.7.3-a3-1"`, new `ensure_migrated()` (per-isolate AtomicBool fast
  path + epoch PK lookup), backfill marker instead of the unindexed
  evidence_log COUNT, `idx_evidence_log_trigger`. Both routes.rs call
  sites (request handler + scheduled) now use `ensure_migrated()`.
- `cargo test --release --test v07_parity`: **21 passed / 0 failed**.
- `cargo check --target wasm32-unknown-unknown -p exchange-worker`:
  **PASS** (2 pre-existing warnings).
- Patched artifact rebuilt and redeployed to staging; health parity
  re-confirmed after redeploy. The patched worker's first cold-isolate
  requests ran full `migrate()` once and stamped the schema epoch +
  backfill marker into the shared D1 — the fix working as designed
  (same marker keys the JS patch uses).

### Burn accounting for the UNPATCHED staged worker (not hidden)

Before the A3 order arrived, I had already sent **≈45 requests** to the
unpatched staged worker (health/capabilities probes, 3 battery passes
that died at W1/W2, parity probes, envelope experiments). The unpatched
Rust `handle_inner` ran `migrate()` on **every** request
(routes.rs:314), so each request burned ≈3,157 D1 rows:
**≈45 × 3,157 ≈ ~140K rows** attributable to this staging session.
(Estimate: request count from session tool calls; per-request cost is
the A3 lane's measured figure.) Requests I sent to *production* ran
under the JS worker's existing per-isolate guard — the pre-existing
production behavior the A3 lane documented, not new burn. Since the
patched redeploy (05:33Z), staged requests pay the gated path
(epoch lookup / warm-isolate 0).

## Step 3 — Staging deploy ✅

- Target: **`lucidota-exchange-staging`** (new script), same D1
  `0ea89801-9b0a-4efa-9d2f-170d7d285e69` as binding `EXCHANGE_DB`,
  compat date 2026-10-01, **no cron trigger set on staging** (deliberate:
  the */30 reaction loop already runs from production against the shared
  D1; a second loop from staging would race it. Cron parity is a
  flip-step concern, not a staging-battery one).
- Tooling: the estate's `~/workspace/exchange/stage/deploy-wasm.py`.
  First attempts failed with Cloudflare error **10021** ("Wasm and blob
  bindings are not supported with modules; use imports instead") — the
  tool declares a `wasm_module` binding that the modules API rejects.
  A debug copy with the binding removed (the wasm part is imported by
  `index.js` as `./index_bg.wasm`) deployed successfully. The estate
  tool itself was not modified; the fixed copy is at
  `~/workspace/.tmp-exchange-test/deploy-wasm-debug.py`.
- workers.dev subdomain: `cf-lanectl.py subdomain-enable` is broken
  (TypeError: body encoding bug); enabled instead via a direct API call
  with the same surrogate credential — `{"enabled": true,
  "previews_enabled": true}`.
- Secrets: `EXCHANGE_KEY` and `PL_SIG_KEY` provisioned with the estate's
  `set-secret.py`, values piped file→stdin from `~/.exchange_key` and
  `~/workspace/keys/PL_SIG_KEY` — never printed, never in argv. Auth
  verified live (no-auth and garbage-bearer both → `unauthorized`;
  real bearer works). During the envelope investigation the staging
  PL_SIG_KEY was temporarily replaced with a throwaway test key and
  then **restored from the real key file** (set-secret confirmed ok).
- Staging URL: https://lucidota-exchange-staging.cutecity.workers.dev
  — left deployed (patched build) for the port owner's debugging;
  owner's call to keep or delete.

## Step 4 — Battery + parity

### Counts parity ✅ EXACT

| Check | Production (baseline) | Staging (Rust, patched) |
|---|---|---|
| `/v1/health` version | 0.7.3 | 0.7.3 |
| resources | 42 | 42 |
| capabilities | 656 | 656 |
| keys | 11 | 11 |
| `/v1/capabilities` total / entries | 656 / 656 | 656 / 656 |
| proven / usable | 9 / 0 | 9 / 0 |
| by_capability | 589 / 25 / 42 | 589 / 25 / 42 |
| by_cost | 102 / 550 / 4 | 102 / 550 / 4 |
| `/v1/config` GET | allow_paid=0, 11 keys | allow_paid=0, 11 keys |

### Config read/write round-trip ✅ (staging)

`GET /v1/config` → `POST /v1/config {scope:"global",
field:"allow_paid", value:0}` (current value written back) →
`changed:true, from:"0", to:"0", log_id:6` → the write is receipted in
`GET /v1/config/log` → re-GET shows allow_paid still 0. Zero net state
change besides the append-only config_log receipt (reason string:
"flip-staging parity round-trip 2026-11-22 (value unchanged)").

### Worker battery (W1–W10) — identical failure on BOTH workers ⚠️ explained

The repo battery (`worker-battery.py`, run with transport-retry added
to its two HTTP helpers — assertions untouched; identical harness for
both targets, differing only in BASE URL) produces the **identical
failure signature against production and staging**: W1 FAIL
(`our_wins=[] all_wins=0`), then a crash at W2 (`KeyError:
'delegation'`) because no claim ever returns a delegation.

Root cause is live state, not the port, proven by direct traces:
**every resource is currently governor-blocked.** `fake-deterministic`
has been `QUARANTINED` since 2026-10-06 (evidence_state QUOTA,
usable=false), and all resources report `usable: 0`. A compute job
submitted to **production** and the same job submitted to **staging**
return state `blocked` with byte-identical governor traces — EVIDENCE
gate fails: "no candidate has verified usable capacity — listings and
trials do not execute". A pinned inference job (groq /
qwen/qwen3.8-27b, the estate's own proof pair) is likewise `blocked`
on both. A claim attempt for groq is refused on both (production 403;
staging 403 / "resource requires local-trust claimant" for the
remote-trust test worker). Claim/complete therefore could not be
exercised end-to-end against *either* worker tonight; the battery
item is satisfied as a parity result (identical behavior), not as a
pass. Re-running the full battery after a resource is re-verified
(evidence gauntlet) remains an open pre-flip item.

### Envelope gate ❌ HARD FAILURE (the flip blocker)

Probes against `/v1/complete` with a nonexistent claim_id (the
envelope gate fires before claim lookup, so this isolates the gate):

| Probe | Production (JS) | Staging (Rust) |
|---|---|---|
| E1 no envelope | 400 missing envelope | 400 missing envelope ✅ |
| E2 valid envelope, real key | **404 unknown claim** (gate passed) | **401 sig mismatch** ❌ |
| E3 envelope bound to another claim | 401 binding mismatch | 401 sig mismatch ❌ |

Discrimination chain (all envelopes verified independently):

1. Python-minted envelope (estate `compute/envelope.py`, the minter
   every live claimant uses): production accepts, staging rejects.
2. The same envelope verifies **OK** under the JS reference verifier
   (`envelope.js verifyEnvelope`) and under the **Rust reference
   verifier** (`pl envelope op=verify` → "envelope chain OK", exit 0),
   both with the key from `~/workspace/keys/PL_SIG_KEY`.
3. A **Rust-minted** envelope (`pl envelope op=wrap`, real key):
   production accepts (404), staging rejects (401 sig mismatch).
4. A throwaway **test key** provisioned onto staging itself via
   set-secret, envelope minted with that test key: staging **still
   rejects** (401); production correctly rejects it too (control).
   Real key restored on staging afterwards.
5. Staging's `EXCHANGE_KEY` works (auth enforced and passing), both
   secrets are present on the script by name (API list), and
   `exchange-worker`'s `build_env` inserts `env.secret("PL_SIG_KEY")`
   verbatim; `exchange-core` `require_envelope` parses it as 64-hex
   (a missing/malformed key would produce 503, not 401).

Conclusion: the defect is inside the deployed Rust worker's envelope
verification path under wasm — it verifies against something other
than the provisioned secret value, or its canonicalization/HMAC
differs under wasm from the native `pl` crate (which passes). It is
**not** a provisioning error, **not** a minter error, and **not** a
stale artifact (a fresh rebuild from patched source reproduces it).
The §6.8 workspace test gate did not catch it because the Rust
suite's envelope tests are native and self-consistent
(Rust-minted → Rust-verified); the failure only appears in the wasm
worker against externally-minted envelopes / its secret path.
Likely first suspects for the port owner: `env.secret()` value
handling in the worker crate version in use, and serde_json map
canonicalization inside `pl::envelope::verify_with` under
wasm32 (e.g. key ordering / number rendering).

## Production safety

- No production deploy, no root `wrangler.toml` change, no git
  commits, no wsign (per orders).
- Final check 2026-11-23T05:29Z: production `/v1/health` =
  v0.7.3 / 42 / 656 / 11; `worker.js` sha256 `76b82928…` and
  `wrangler.toml` sha256 `f8a2f13b…` — identical to the step-1 backup.
- Shared-D1 writes made by this session, all disclosed: staging-era
  test jobs (blocked, inert), battery test jobs (blocked, inert), one
  no-op config write (allow_paid 0→0, config_log id 6), envelope
  rejection records from the probes, the A3 schema-epoch stamp +
  backfill marker + `idx_evidence_log_trigger` from the patched
  worker's first migrate, and the D1 mirror refresh (read-only
  snapshot).

## What must happen before step 5 can be reconsidered

1. Fix the wasm envelope verification defect; re-stage and re-run
   probes E1–E3 — E2 must return 404 and E3 the binding-mismatch 401.
2. Re-verify at least one resource (evidence gauntlet) and run the
   full W1–W10 battery + a live pinned claim/complete against staging.
3. Confirm the JS-side A3 patch deployment state so both
   implementations agree on schema epoch `v0.7.3-a3-1` at flip time.
