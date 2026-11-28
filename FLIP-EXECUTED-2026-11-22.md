# Exchange Flip — EXECUTED (FLIP-READINESS §7 steps 5–7)

Executed 2026-11-23 ~05:55–05:58 UTC by the flip-exec agent.
Pre-flip gates: FLIP-STAGING-2026-11-22.md (staging parity),
ENVELOPE-DEFECT-2026-11-22.md (envelope fix, wasm sha `cd52d7e8…`),
FLIP-FINAL-PREP-2026-11-22.md (A3 JS live, gauntlet re-verified,
staging live cycle). All three were closed before this run.

## VERDICT: FLIP COMPLETE — Rust worker live on production, all verification green, no rollback triggered

**Deploy time (UTC): 2026-11-23T05:56:31Z** (PUT started 05:56:31Z,
success response 05:56:34Z). Owner: check the first `*/30` cron cycle
against this time — the first scheduled boundary after deploy is
**2026-11-23T06:00:00Z**. Per §7 step 6, the flip is declared done on
the strength of the verification below; the one-cron-cycle monitoring
of health / claim-complete success / D1 read-burn is the owner's
confirmation pass (the pre-existing D1 daily-burn caveat from the A3
work applies unchanged — the Rust worker runs the A3 gated
`ensure_migrated()` path, epoch PK lookup per cold isolate, so the
flip does not change the burn profile the A3 patch established).

## Step 5.1 — Fresh backup (rollback target) ✅

`~/workspace/exchange/backups/2026-11-22-flip/`:
- `worker.js` — sha256
  `57026745503f2f29016c78ea46682a98aebdfad406df77f9d5914d307919acf8`
  (the A3-patched production JS, copied from the live tree at
  05:55Z, immediately before the flip)
- `wrangler.toml` — sha256
  `f8a2f13b2c2ba112ac3bea65469ce1f1ff57a78d41035c96d4988f6603827aa5`
- `PARITY-BASELINE.json` — pre-flip production state (below)

Pre-flip baseline (2026-11-23T05:56:01Z, worker.js still live):
health 200 / version 0.7.3 / resources 42 / capabilities 656 /
keys 11; `/v1/capabilities` total 656, entries 656, proven 9,
usable 0, by_capability {inference 589, embedding 25, other 42},
by_cost {free 102, paid 550, unfunded 4}; `/v1/config`
allow_paid=0, 11 keys. Script settings (API): bindings EXCHANGE_DB
(d1, `0ea89801-9b0a-4efa-9d2f-170d7d285e69`), EXCHANGE_KEY
(secret_text), PL_SIG_KEY (secret_text); crons `["*/30 * * * *"]`.

## Step 5.2 — Flip config carry-over ✅ verified before and after

The flip config (`rs/exchange-worker/wrangler.toml`) carries:
name `lucidota-exchange`, account `243aabef9144fd3b8dd334cb19319e75`,
compatibility_date 2026-10-01, D1 binding `EXCHANGE_DB` →
`0ea89801-9b0a-4efa-9d2f-170d7d285e69`, triggers crons
`["*/30 * * * *"]` — identical to the production root wrangler.toml
on every field that matters. (Staging deliberately ran with no cron;
the flip config has it.)

Post-deploy settings re-read (API, names/types only — no secret
values ever printed or read back):
- bindings: EXCHANGE_DB (d1, same id), EXCHANGE_KEY (secret_text),
  PL_SIG_KEY (secret_text) — **both secrets persisted across the
  upload**; no re-provisioning was needed.
- crons: `["*/30 * * * *"]` — **the production cron trigger is
  present on the Rust worker.**

## Step 5.3 — Deploy ✅

Artifact: `~/workspace/exchange/rs/exchange-worker/build/`
(envelope-fixed build) — `index.js` sha256 `0c3c24f0…`,
`index_bg.wasm` 1,741,900 B sha256
`cd52d7e8f367b853a128c9839844acc6699c8c6bbbb461d8a49d2c11a897038d`
(both re-verified by hash at the start of this run).
Tooling: the estate debug deploy copy
(`~/workspace/.tmp-exchange-test/deploy-wasm-debug.py` — the
`stage/deploy-wasm.py` variant without the `wasm_module` binding the
modules API rejects with error 10021; the wasm part is imported by
`index.js` as `./index_bg.wasm`), script `lucidota-exchange`,
D1 binding `0ea89801…:EXCHANGE_DB`, compat date 2026-10-01.
Result: `{"success": true, "script": "lucidota-exchange"}`.

## Step 5.4 — Post-flip verification (in order, all against production)

1. **GET /v1/health** → HTTP 200,
   `{"ok":true,"version":"0.7.3","resources":42,"capabilities":656,"keys":11}`
   at 2026-11-23T05:56:43Z (9 s after deploy). Counts match baseline
   exactly. Repeated at 05:57:03Z and 05:57:23Z — identical.
2. **GET /v1/capabilities** → total **656**, entries **656**,
   proven 9, usable 0, by_capability {589/25/42}, by_cost
   {102/550/4} — identical to baseline. (Fetched via curl: urllib
   truncates this ~230 KB response on the VM→Worker path, the known
   F10 fault; the truncated attempts are counted in the disclosure.)
3. **GET /v1/config** → HTTP 200, `allow_paid=0`, 11 keys — the Rust
   worker reads the live D1 config tables; state sane and unchanged.
4. **Live cycle on production** (fake-deterministic, the estate's
   deterministic proof resource, re-verified AVAILABLE in
   FLIP-FINAL-PREP):
   - `POST /v1/job` {capability: compute, input: {op: sha256, data:
     "flip-executed-2026-11-22"}, idempotency_key: flip-exec-1} →
     200, job `job_flip-exec-1`, state `deferred` (delegated).
   - `POST /v1/claim` (fake-worker-01) → 200, claimed our job on the
     first call, claim `claim_mwsusr68d7lfbo`, lease
     `lease_mwsusoxvff4unp`. No foreign claims needed releasing.
   - Executed deterministically (sha256) and `POST /v1/complete`
     with a valid `lucidota-receipt/1` envelope → 200, state `done`,
     taxonomy `ok`, worker_outcome `SUCCEEDED`, receipt
     `rcpt_mwsussnov9v9x6`.
   - `GET /v1/receipts/rcpt_mwsussnov9v9x6/verify` → HTTP 200,
     **`verified: true`** (input_hash, output_hash and the remaining
     checks pass — same check set proven on staging).
5. **Tampered-envelope complete** → envelope minted for a bogus
   claim, payload mutated after signing, `POST /v1/complete` →
   **HTTP 401** `envelope rejected: envelope: sig mismatch —
   envelope NOT sealed by the authority key`. The Rust worker fails
   closed with the exact production-correct error (the pre-fix
   defect's signature error, now produced only for genuinely invalid
   signatures).

No 5xx was observed in any request this session. No verification
failure, no health count drift → **rollback trigger never fired.**

## Request / D1 burn disclosure (this session)

Cloudflare API calls (no D1): script settings GET ×2, schedules
GET ×2 (both inside cf_state runs), script PUT ×1 (the deploy).
Worker HTTP requests to production: health ×4 (baseline, post-flip
check, cycle-script check, final), capabilities ×6 (4 urllib
attempts truncated by F10 pre-flip — each ran server-side; 1 curl
pre-flip, 1 curl post-flip), config GET ×2 (pre/post), job submit
×1, claim ×1, complete ×2 (1 valid, 1 tamper probe), receipt verify
×1. Total ≈ 17 worker requests. Per-request D1 cost is the normal
route cost under the A3 gated path (health ≈709 rows is documented
pre-existing behavior, unchanged by the flip; claim path ≈34 rows);
the one-time full migrate did not re-run — the schema epoch
`v0.7.3-a3-1` was already stamped and both implementations share it.
Deliberate D1 writes: job `job_flip-exec-1` + its lease/claim/
execution/receipt rows (all `done`/`ok`), one envelope_rejections
row from the tamper probe, one seal_registry insert from the valid
complete (replay registration, by design). No config writes.

## Rollback coordinates (if the owner's cron-cycle check fails)

1. The rollback target is `backups/2026-11-22-flip/worker.js`
   (sha256 `57026745…`). The live tree's `~/workspace/exchange/worker.js`
   is byte-identical to it (re-verified at 05:57Z) and was not
   modified by this flip.
2. Roll back: `python3 ~/workspace/exchange/deploy-exchange-070.py`
   from `~/workspace/exchange/` (uploads worker.js + envelope.js,
   verifies schedules unchanged, never touches secrets). If the live
   tree's worker.js has changed since, first restore it from the
   backup copy above and restore the root `wrangler.toml` from the
   same backup directory.
3. Verify: `GET /v1/health` back to the JS baseline (200 / 0.7.3 /
   42 / 656 / 11 — the counts are implementation-independent), and
   re-check `/v1/config/log` for any config written during the Rust
   window (this session wrote none).
4. No D1 rollback is needed or possible: all schema changes are
   additive and the JS worker tolerates the Rust era's rows
   (FLIP-READINESS §7 step 7).

## Production safety / leftovers

- The Rust worker is **live** on `lucidota-exchange` as of
  2026-11-23T05:56:31Z. Root `wrangler.toml` and the on-disk
  `worker.js` were not modified by the flip (the deploy is an API
  script upload; the root config still names worker.js, which is
  precisely what makes the rollback above a one-command operation).
- Staging (`lucidota-exchange-staging`, same envelope-fixed build)
  is still deployed, unchanged by this run — owner's call to keep
  or delete (unchanged from FLIP-STAGING).
- No git commits, no wsign (owner signs this flip record).
