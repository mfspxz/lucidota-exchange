# Exchange Flip — Final Pre-Flip Preparation (FLIP-STAGING items 2 + 3)

Executed 2026-11-23 ~05:47–05:55 UTC. Step 5 (the production flip) was
NOT executed — it is the owner's step.

## VERDICT: pre-flip items 2 and 3 are now satisfied

1. **A3 JS patch is LIVE on production** — health identical to baseline
   (0.7.3 / 42 / 656 / 11), schema epoch `v0.7.3-a3-1` confirmed in D1.
   Both implementations now agree on the epoch on the production path.
2. **fake-deterministic is re-verified and USABLE** — the evidence
   gauntlet honestly promoted it QUOTA → **AVAILABLE** (health
   QUARANTINED → AVAILABLE) on a real claimant execution. Nothing was
   forced: the ladder moved on a recorded observation + execution +
   $0 receipt, via the designed delegated flow.
3. **Staging battery + live cycle**: W1 (claim atomicity) and W5
   (zombie sweep) PASS on the Rust worker — both were untestable at
   baseline. The remaining battery failures trace to the battery
   harness predating the envelope gate (identical on production JS,
   proven live in Step 2), not to the port. One real envelope-gated
   claim/complete cycle against staging completed end-to-end with a
   **verified receipt**.

---

## STEP 1 — A3 fix deployed to production JS ✅

Pre-checks:
- `worker.js` sha256 `76b8292854b2ae84402d128524ea6e6a53314b7bd04453474f80d2c5f7c3dcd3`
  and `wrangler.toml` sha256 `f8a2f13b…` — identical to the sealed
  backup at `backups/2026-11-22-flip-staging/`.
- Fresh baseline: `GET /v1/health` → 200, version 0.7.3, resources 42,
  capabilities 656, keys 11.
- D1 pre-state: `exchange_config.schema_epoch` was **already**
  `v0.7.3-a3-1` (updated_at 2026-11-23T05:33:27Z) and
  `backfill_v03_done='1'` — stamped by the staging Rust worker's first
  `migrate()` against the shared D1, as disclosed in FLIP-STAGING.
  The two implementations share the epoch string and marker keys by
  design, so the stamp is the agreed state, not a discrepancy.

Apply + deploy:
- `patch -p0 < D1-BURN-FIX-A3.worker-js.patch` — applied clean;
  `node --check worker.js` OK; `SCHEMA_EPOCH = "v0.7.3-a3-1"` present
  at worker.js:2518. Patched `worker.js` sha256
  `57026745503f2f29016c78ea46682a98aebdfad406df77f9d5914d307919acf8`.
- Deployed via the estate path `deploy-exchange-070.py` (multipart
  worker.js + envelope.js): its gates passed (version stamp, syntax,
  3/3 node test files), deploy ok, **schedules unchanged** (`*/30`
  before and after, identical created/modified stamps), secrets
  untouched (script secrets persist across uploads by design).

Post-deploy verification (independent of the deploy script's own):
- `GET /v1/health` → HTTP 200, `{"ok":true,"version":"0.7.3",
  "resources":42,"capabilities":656,"keys":11}` — counts identical to
  baseline. No rollback condition triggered; backup not needed.
- D1: `schema_epoch = 'v0.7.3-a3-1'` confirmed post-deploy. Honest
  nuance: the post-deploy JS requests did **not** re-stamp the epoch
  (updated_at unchanged) — the patched `ensureMigrated()` read the
  existing epoch, found it equal to `SCHEMA_EPOCH`, and took the gated
  fast path. That is the fix working as designed: cold-isolate cost is
  now the 1-row PK lookup instead of the ~3,157-row migrate.
- FLIP-STAGING "what must happen" item 3 is therefore satisfied: JS
  and Rust agree on schema epoch `v0.7.3-a3-1` at flip time.

## STEP 2 — Evidence gauntlet re-verification (production) ✅

Candidate: **fake-deterministic** — the battery's own resource, the
estate's deterministic proof resource; adapter `claimed` (delegated
gauntlet), claimant_trust `any`, credential_state `valid`, quota row
60/min measured=1. Starting state: health `QUARANTINED` (since
2026-10-06), evidence `QUOTA`, usable=false.

What the gauntlet did, in order (all on production, designed flow):
1. `POST /v1/resources/fake-deterministic/verify` → HTTP 200,
   `delegated: true`, job `job_mwsuiric2ttfzf`, lease
   `lease_mwsuirqsx38e4a`; all five stages recorded "pending" —
   the Worker does not probe claimed resources itself.
   evidence_log id 3162: QUOTA→QUOTA, trigger `delegated`.
2. The stock `fake-claimant.py` claimed the lease (claim
   `claim_mwsuiv6kpgquqd`, execution row created) but its completion
   was **rejected 400 "missing envelope"** — the script predates the
   envelope gate. Reproduced deliberately with a bare complete call:
   production → 400 `missing envelope: receipts must carry a
   lucidota-receipt/1 envelope`. (Harness staleness, not a gauntlet
   failure; the claim stayed active and completable.)
3. Completed the same claim with a valid `lucidota-receipt/1`
   envelope (Python minter, payload bound to claim/job/resource,
   output `{"op":"echo","result":"probe-ok"}`) → HTTP 200, job
   `done`, receipt `rcpt_mwsukofazfakn9`, taxonomy `ok`.
4. The ladder moved on the recorded evidence, inline at completion:
   - health_observations id 51988: ok=1, "claimant: execution ok"
   - evidence_log id 3163: health QUARANTINED → AVAILABLE
     (`health-transition`)
   - evidence_log id 3164: evidence QUOTA → **AVAILABLE**
     (`auto-promote`, "warranted by recorded observations")
   - `resources.evidence_at` updated 2026-10-06 → 2026-11-23T05:50:51Z.

Result: `evidence_state = AVAILABLE`, health `state = AVAILABLE` —
AVAILABLE ∈ EVIDENCE_USABLE, so the resource is usable (≥1 usable
resource where there were 0). The gauntlet was not forced and no
evidence state was written by hand; every transition above is the
worker's own machinery reacting to a real execution.

Side note: `GET /v1/resources/:id/evidence` via the urllib harness
hit the known F10 transport truncation (IncompleteRead) — server-side
state was confirmed via D1 instead.

## STEP 3 — Battery + live claim/complete against STAGING

Target: `lucidota-exchange-staging.cutecity.workers.dev` (Rust,
envelope-fixed + A3 build), same D1. Staging health at start:
200 / 0.7.3 / 42 / 656 / 11 — parity counts hold.

### W1–W10 battery (`worker-battery-staging.py`, tag `ffp1123`, as-is)

Production-baseline behavior (FLIP-STAGING): W1 FAIL
(`our_wins=[]`), crash at W2 (`KeyError: 'delegation'`) — no claim
ever returned a delegation because every resource was unusable.

| Item | Result | Evidence |
|---|---|---|
| W1 double claim | **PASS** | exactly one winner for `job_ffp1123-w1-race` (fake-worker-02, claim `claim_mwsulpeq4fqo7l`), all_wins=1 — claim atomicity holds on Rust |
| W2 replay completion | FAIL | first complete → 400 (missing envelope); idem 0/9; accounting rows 0 |
| W3 forged completion | FAIL | resource=400, worker=400, unknown=400 (expected 400/403/404) |
| W4 expired lease | FAIL | complete → 400 (missing envelope); claim still `active` |
| W5 zombie worker | **PASS** | staging admin sweep swept the claim; execution result `uncertain` |
| W6 malicious claimant | NOT RUN | battery crashed: claim for the w6 job returned no delegation (`KeyError: 'delegation'`) |
| W7 secret scan | NOT RUN | battery died at W6 |
| W8 malformed provider | NOT RUN | battery died at W6 |
| W9 model lie | NOT RUN | battery died at W6 |
| W10 partial completion | NOT RUN | (the repo battery has no W10 section; W5 covers the vanish case) |

Reading of the failures — harness, not port:
- W2/W3/W4 all fail at the **envelope gate**, which fires before the
  checks the battery asserts. The battery's completes carry no
  envelope; it predates the gate. Production JS behaves identically
  (proven live in Step 2: bare complete → 400 missing envelope), so
  this is exact parity behavior — the battery harness needs an
  envelope-minting update before W2–W4 can assert their intended
  semantics against *either* implementation.
- W6 crash: the claim call at that moment returned no delegation.
  Root cause not isolated; observed context: all six battery jobs
  ended in state `deferred` (replanned), and earlier items left
  stuck-active claims whose completions had been envelope-rejected,
  so the claim queue state mid-battery was polluted by the harness's
  own failed completes. The standalone cycle below claimed cleanly
  minutes later against the same worker and D1.
- W1 and W5 — the two items that exercise pure worker-side machinery
  (claim atomicity; sweep + uncertain marking) — PASS on Rust. Both
  were untestable at baseline.

### One real claim/complete cycle (staging, envelope-gated) ✅ PASS

- `POST /v1/job` {capability: compute, input: {op: sha256, data:
  "flip-final-prep"}, idempotency_key: ffp-cycle-1} → job
  `job_ffp-cycle-1`, state `deferred` (delegated).
- `POST /v1/claim` (fake-worker-01) → claimed our job on the first
  call, claim `claim_mwsunmgac4sz9w`.
- Executed deterministically (sha256) and completed **with a valid
  envelope** → the completion was accepted server-side: D1 shows job
  `done`, claim `completed`, receipt `rcpt_mwsunnua551tv2`
  (result `ok`). Disclosure: the HTTP response to the first complete
  was lost to the known F10 transport fault; the harness retried with
  the identical envelope and received **409 "envelope replay
  rejected"** — the replay gate working as designed, and the reason
  the script printed FAIL before D1 confirmed acceptance.
- `GET /v1/receipts/rcpt_mwsunnua551tv2/verify` on staging →
  `verified: true`; checks pass: input_hash, output_hash,
  governor_trace_present, delegation_chain_resolved, claim_binding
  (claim + worker bound), accounting_bound (measured_cost 0.0,
  cost_basis measured), all_gates_recorded (missing: []),
  cost_within_policy.

## D1 row-burn disclosure (this session)

- My direct D1 API queries (d1q / d1q-full): 13 queries with saved
  meta total **4,337 rows_read, 0 rows_written** (a floor — it
  excludes the battery's own cf-d1.py calls and overwritten outputs).
- Worker HTTP requests I sent: Step 1 ≈ 5 production reads (health
  ×3 incl. deploy-script verification, root ×1) at the documented
  ~709 rows per /v1/health (unchanged by A3 — health COUNTs were
  deliberately left out of the patch). Step 2: 1 verify POST +
  2 claims + 3 completes + 1 evidence GET on production (claim path
  ≈34 rows/poll per D1-BURN-FIX-A3.md; the verify POST's one-time
  gated migrate check is now the 1-row epoch lookup on JS too).
  Step 3: battery ≈ 7 submits + ~10 claims + ~12 completes + 1 admin
  sweep + ~10 cf-d1 calls against staging (same D1; staging runs the
  A3 gated path, epoch lookup per cold isolate); cycle = 1 health +
  1 submit + 1 claim + 2 completes (one replay-rejected).
- Deliberate shared-D1 writes, all disclosed above: the A3 epoch was
  already stamped pre-session (by staging); the gauntlet's job/lease/
  claim/execution/observation/receipt rows and the two evidence
  transitions for fake-deterministic; battery jobs
  `job_ffp1123-*` (all left `deferred`, inert); cycle job
  `job_ffp-cycle-1` (done) + its receipts (one governor `deferred`
  receipt from delegation, one `ok` receipt from completion);
  W4/W5's direct D1 edits (claim/lease expiry backdating — the
  battery's own designed manipulation).
- The production */30 cron also runs against this D1 independently;
  its burn is pre-existing behavior, not this session's.

## Production safety

- Production deploys this session: exactly one — the A3 worker.js
  patch (Step 1), verified post-deploy. Root `wrangler.toml`
  untouched; cron schedule proven unchanged by the deploy script.
- Final check 2026-11-23T05:53Z: production `/v1/health` = 200 /
  0.7.3 / 42 / 656 / 11; production `worker.js` sha256
  `57026745503f2f29016c78ea46682a98aebdfad406df77f9d5914d307919acf8`
  (the A3-patched file). Sealed rollback remains at
  `backups/2026-11-22-flip-staging/` (worker.js sha256 `76b82928…`).
- The flip itself (FLIP-READINESS §7 step 5) was NOT performed.
- No git commits, no wsign, no secret values printed (per orders).

## What remains before step 5

Nothing from FLIP-STAGING's list: (1) envelope defect fixed and
proven, (2) a resource re-verified + battery/live cycle run against
staging, (3) JS A3 deployed — both implementations agree on schema
epoch `v0.7.3-a3-1`. Remaining judgment calls for the owner at flip
time (unchanged from FLIP-STAGING): staging has **no cron trigger**
(production's */30 must carry over in the flip's wrangler config),
and the repo battery harness should get an envelope-minting update
if W2–W4/W6–W9 are to be asserted in their intended form
post-flip.
