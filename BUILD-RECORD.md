# EXCHANGE v0.4 — BUILD RECORD (CONTROL PLANE)
**Built:** 2026-10-06 | **By:** Lucidota | **Directive:** Zackary's "COMPLETE THE EXCHANGE" — one coherent control plane implementing DISCOVER → PROVE → AUTHORIZE → RESERVE → EXECUTE → VERIFY → ACCOUNT → LEARN → REPLAN

## v0.4 changes (all live)
1. **Hard pre-execution gates (10, in order)** — IDENTITY → AUTHORITY → DATA → CAPABILITY → EVIDENCE → QUOTA → COST → INTEGRITY → LEASE → EXECUTE. Every gate recorded in the trace; unreached gates recorded as explicitly-labeled "vacuous", never faked. No score, retry, failover, urgency, or human convenience bypasses a gate.
2. **Lease before side effect** — leases table extended (authority, expected_cost, quota_snapshot, policy_version, expires_at ~60s). Created after gates pass; health/quota/cost/evidence revalidated from fresh reads immediately before dispatch. Expired/lost/invalidated lease → discard + replan, never execute.
3. **Attempts ledger + failure taxonomy** — executions table is the attempts ledger (attempt_no, lease_id, failure_taxonomy, request_hash). Taxonomy: RETRYABLE_FAILURE | PERMANENT_FAILURE | POLICY_FAILURE | AUTH_FAILURE | QUOTA_FAILURE | CAPABILITY_FAILURE | PROVIDER_FAILURE | VERIFICATION_FAILURE, mapped from adapter error classes (new: AUTH_ERROR on 401/403, VERIFICATION_FAILED on contract-verify failures). Never collapsed into "failed".
4. **Receipts as system of record** — receipts now carry attempt_id, policy_version (sha256 of canonical policy set), lease snapshot, quota_before/after, cost_before/after, health_before/after, fallback_chain. Receipt verify checks all 10 gates + policy version + lease snapshot + execution-verified (no success reported without a passing EXECUTE gate).
5. **Reaction loop** — cron is now OBSERVE → CLASSIFY → UPDATE → REPLAN: THROTTLED with exponential backoff on 429s (2^(n-1) min, cap 60), EXHAUSTED suspends until recheck (5 min), AUTH_ERROR disables credential (PARKED until explicit re-verify), provider-down detection (NETWORK on all probeable resources → provider DOWN, resources UNKNOWN), deferred-job replan (bounded 2/run, 8 defers max), orphan reaper (stale 'running' jobs with no receipt → released + deferred). Every transition → evidence_log.
6. **Honest terminal states** — jobs end done | failed | blocked | deferred. All-retryable rejections → deferred (cron replans); policy/capability → blocked; mixed/permanent → failed with full taxonomy chain. Every terminal state writes a receipt.
7. **Economic governor** — scoring adds effective_cost, expected_success (reliability), human_intervention_cost (untrusted/external penalty). Paid resources may register but can NEVER execute without explicit job.spend_authority > 0; default $0 stands. New route GET /v1/jobs?state= for deferred-queue visibility.
8. **Learning/policy separation (invariant, in code)** — observations may write only health/quota/evidence/accounting columns, never the policies table (verified: only seed() INSERT OR IGNOREs defaults). Evidence updates knowledge; policy controls permission.

## v0.4 test evidence (this session, live D1)
- T1 real inference → lane-kilo 429'd mid-run (QUOTA_FAILURE, lease released, attempt recorded) → job honestly deferred, not faked; verify gauntlet restored lane-kilo (ACCESSIBLE→FREE→QUOTA→AVAILABLE, headroom 10/10 measured)
- T2 sensitive inference → BLOCKED at DATA gate, POLICY_FAILURE taxonomy, receipt verified True (all 10 gates recorded incl. vacuous labels)
- T3 receipt verify → independent pass on blocked receipt (input_hash, trace, policy_version, fallback_chain)
- T4 deferred replan → cron picks up deferred jobs against current reality (bounded)
- Adversarial finds during build: (a) 15-placeholder/14-bind receipt insert left a job stuck in 'running' → fixed + added orphan reaper; (b) two missing .bind() on job-detail route (one pre-existing v0.3) → fixed + added whole-file bind-count checker

## v0.3 (shipped earlier this session, not previously recorded)
Evidence lifecycle DISCOVERED → ACCESSIBLE → FREE → QUOTA → AVAILABLE → VERIFIED; EVIDENCE gate in governor; POST /v1/discover, POST /v1/resources/:id/verify, GET /v1/resources/:id/evidence, GET /v1/capacity; tier-based cron probing; billing-appeared demotion; discovery-source ingestion.

## v0.2 (preserved, see below)

---

# EXCHANGE v0.6 — BUILD RECORD (Worker Protocol v1 — FROZEN)
**Built:** 2026-10-06 | **By:** Lucidota | **Directive:** Zackary systems-lead — harden the worker contract until an arbitrary claimant swaps in with zero governor changes

## Doctrine
Governor owns authority (gates, leases, accounting, receipts). Worker owns execution (pickup, validate, execute, capture, report). Neither impersonates the other. Worker output is evidence; receipt verification decides what the Exchange believes.

## v0.6 changes (all live)
1. **Worker identity** — `POST /v1/workers/register` (governor admin) mints worker_id + secret (shown once, sha256 stored). Worker endpoints (`/v1/claim`, `/v1/complete`) moved BEFORE the Exchange-key gate and use `X-Worker-Id`/`X-Worker-Secret`. A worker holding the master key was a doctrine violation — closed.
2. **Claims table** — JOB → CLAIM → EXECUTION → RECEIPT → ACCOUNTING are distinct objects. Claim binds worker↔lease with policy_snapshot, receipt_requirements, idempotency_key.
3. **The delegation object** — claim_id, resource_id, capability, input, policy_snapshot, lease_expiry, attempt, idempotency_key, receipt_requirements, credential_ref (opaque — the secret never travels), requested_model.
4. **5 terminal outcomes** — SUCCEEDED | FAILED_RETRYABLE | FAILED_PERMANENT | QUARANTINED | EXPIRED. Anything else → 400. Boring worker: no retries, no failover, no cost assertion.
5. **10 invariants enforced** — no lease minting, no gate bypass, no self-accounting (W6: cost=$99999 ignored, books show measured), no arbitrary completion (W3: wrong worker/resource → 403/400), no secret access, no failure→success conversion (W8: empty 200 → VERIFICATION_FAILURE), no receipt manufacture, no lease extension (W4: → LEASE_EXPIRED), no policy change.
6. **Idempotent completion** (W2) — replays return the original; single accounting row.
7. **Atomic claim** (W1) — one conditional UPDATE decides the winner; thread-race proven.
8. **Zombie sweep** (W5/W10) — expired claims → EXPIRED, execution UNCERTAIN (provider may have executed — retries not claimed free), job replanned. Cron + admin endpoint.
9. **Accounting table** — governor's books, separate from receipts. (Two early rows missing from the initial deploy window; receipts intact — noted, not backfilled.)
10. **Requested-vs-actual binding** (W9) — execution records requested_model/actual_model/model_match; receipt exposes discrepancy.
11. **Trust gates** — resources declare `claimant_trust_required` (local|any). nim-nvidia requires local; remote workers get 403.
12. **Fixed the duplicate-lease defect** — runJob created an inline lease+execution THEN delegated (two leases, one orphan). Delegation now owns the lease exclusively.
13. **WORK AVAILABLE seam** — documented: poll (current hook) is one implementation; socket/callback/queue/systemd/websocket/push are future. Worker never knows.
14. **Hook simplified** — runs the boring worker directly every 120s; no agent wake for deterministic work. Dry-run guard on state writes.

## Proofs (all live, 2026-10-06)
- **FAKE_CLAIMANT**: arbitrary deterministic worker (sha256/echo) ran the FULL loop — discover → verify (delegated gauntlet) → gates → lease → claim → execution → completion → receipt → accounting → capacity (DISCOVERED → AVAILABLE) — with **zero governor changes**. The worker boundary is proven.
- **W1–W10 battery**: 9/9 PASS (W10's uncertain-execution covered by W5). Receipts/claims captured for each.
- **NIM v1 loop**: job → delegation → nim-claimant-01 (local trust) → SUCCEEDED → done, receipt verified, accounting $0 measured.
- Protocol frozen in `WORKER-PROTOCOL-v1.md` — 18/18 acceptance bar checked.

# EXCHANGE v0.5 — BUILD RECORD (archived)
**Built:** 2026-10-06 | **By:** Lucidota | **Directive:** "Use Nvidia" — bridge the vault-held NIM credential

## v0.5 changes (all live)
1. **`claimed` adapter** — delegated execution for credentials the Worker must never see. The Worker governs (gates, lease, accounting); a trusted claimant holding the vault credential executes via `/v1/claim` + `/v1/complete`. Atomic claim (single winner), 10-min pickup window, full receipt accounting on completion mirroring inline execution.
2. **NIM is a live Exchange resource** — `nim-nvidia` (provider `nvidia`), trust `keyed`, evidence AVAILABLE, cost measured $0. Real inference job `job_nim-proof-001` → done on NIM ("The capital of Canada is Ottawa."), receipt independently verified 10/10.
3. **Delegated verify gauntlet** — `POST /v1/resources/:id/verify` on claimed resources creates a real verification job through claim/complete; every evidence tier is earned from recorded observations/receipts, no shortcuts.
4. **Cron integration** — never probes claimed resources (claimant observations are the probe); auto-promotes from them; never replans a live delegation (no double-execution).
5. **Verifier follows the delegation chain** — completion receipts resolve the 10 governor gates from the delegation receipt; `execution_verified` via the VERIFY gate.
6. **Hook `nim-claimant`** (every 120s) — polls `/v1/claim`, wakes a worker agent only when delegated work exists or a 6h freshness probe is due. Steady state costs zero agent runs.
7. **`discover-local.py`** — the systemic fix: scans workspace skills for vault-connected APIs and registers missing ones as DISCOVERED leads. Found deepseek, mistral, ai-router, openrouter (+nvidia/cloudflare already known).
8. **Bug fixed:** D1 reports changed rows at `result.meta.changes`, not `result.changes` — the claim endpoint always read "lost race" while actually claiming the lease. Also: edge 403s bare Python-urllib UA; scripts now send a proper User-Agent.
9. **NIM model reality (measured, not catalog):** catalog lists ~81 models but per-account availability differs. `openai/gpt-oss-20b` → 200 in ~1s (reasoning model: answer in `reasoning_content`, `content` null — claimant extracts with fallback). `deepseek-ai/deepseek-v4.1-flash` hangs; gemma/mistral/llama-3.1 IDs 404/410 (EOL or not deployed for account).

## v0.5.1 — red-team fixes (2026-10-06, same deploy line)
Adversarial agent vs live: auth, idempotency, sensitive blocking, honest terminals, backoff, taxonomy, quarantine, receipt verification all PASS. It fixed 2 bugs itself (error-class prefix bypass defeating 429-backoff/credential-disable; verify clobbering PARKED→UNKNOWN — both live). Found 3 more, all fixed:
1. **LIVE DEGRADATION:** QUOTA gate used `LIKE 'RATE_LIMITED%'` over ALL history — one synthetic 429 permanently bricked cutecity-health. Now substring match + 60-min decay; deleted the synthetic row; cutecity-health unbricked (http-check done).
2. billingCheck demoted paid executions on FREE/QUOTA/AVAILABLE/VERIFIED → ACCESSIBLE. Now only demotes `cost_class==='free'` — paid resources with spend_authority stay usable.
3. Mixed retryable+non-retryable rejections → blocked, starving REPLAN. Now defer when ANY rejection is retryable (defer_count<8 bounds it); all-permanent still blocks.
12 labeled `redteam-*` synthetic resources remain in D1, all unusable — harmless.

## v0.4 (builder agent, this session)
10-gate control plane (IDENTITY→…→LEASE→EXECUTE), leases with 60s expiry + revalidation, 8-class failure taxonomy, receipts as system of record, economic governor ($0 default, paid needs spend_authority), reaction loop (throttle/exhaust/replan/orphan-reaper), observation/policy/score separation. Live adversarial proof: lane-kilo 429'd → deferred → PERMANENT_FAILURE → quarantined → honestly reported zero inference capacity.

# EXCHANGE v0.2 — BUILD RECORD (archived)
**Built:** 2026-10-06 | **By:** Lucidota | **Directive:** constitutional hardening (5 rules)

## v0.2 changes (all live)
1. **Adapter registry by config** — `config.adapter` selects the adapter (kind as fallback). New substrates register via API without code deploy. Adapters: `ai-lane`, `http-post`, `http-get`, `self`.
2. **Generic Job Contract proven** — `http-check` (cutecity.cc monitoring) and `ingest` (ctcc-router) capabilities live alongside `inference`. AI is one workload among many.
3. **Error classification** — every execution records error_class: RATE_LIMITED | TIMEOUT | SERVER_ERROR | BAD_RESPONSE | NETWORK | POLICY_BLOCKED | UNKNOWN. Verified live (BAD_RESPONSE on dead endpoint).
4. **Receipt verification** — GET /v1/receipts/:id/verify recomputes input_hash, checks governor trace completeness, verifies cost policy. Returns independent pass/fail.
5. **Resource registration API** — POST /v1/resources registers provider/resource/capabilities/quota with adapter validation. Unknown adapters rejected with the known list.
6. **Smarter cron** — probes only UNKNOWN, DEGRADED, QUARANTINED, or stale (>15 min) resources. Healthy+fresh skipped.
7. **Honest seed** — providers start UNKNOWN, not AVAILABLE. GitHub provider registered as UNKNOWN with exact missing-access note.
8. **New live resources** — ctcc-heartbeat (ingest), cutecity-health (http-check), tricycles-page (http-check).

## v0.1 (preserved)
Worker + D1 exchange DB, 8-gate governor, deterministic scoring, failover loop, receipts, $0 cost rule, 5-min cron, EXCHANGE_KEY fail-closed. All still working, none rewritten.

## Test evidence (this session, all in D1 receipts)
- http-check on cutecity.cc → done, verified database=true, receipt verified independently
- inference with all lanes quarantined → blocked (honest terminal state)
- bad adapter registration → rejected with known-adapter list
- malformed job → 400 with reason
- quarantine → recovery → successful inference (full cycle)
- error classification → BAD_RESPONSE recorded on dead endpoint
- dead-test resource retired (QUARANTINED, audit trail preserved — FK integrity kept over cosmetic deletion)

## Still BLOCKED (unchanged, not Exchange dependencies)
- R2 assetStorage repair: token 403 (Cute City debt, not Exchange)
- External providers: need his accounts
- 4 workers: SOURCE LOST, artifacts preserved
- Token broadening: NOT done, per his guidance — Exchange doesn't need it

## What was built
`lucidota-exchange` — Cloudflare Worker + D1 `exchange` database.
URL: https://lucidota-exchange.cutecity.workers.dev
Source: ~/workspace/exchange/worker.js (~1000 lines, single file)
D1: exchange (0ea89801-9b0a-4efa-9d2f-170d7d285e69) — 10 tables

## Components (all implemented, all live)
1. **Resource ontology** — providers, resources, resource_capabilities, quotas, policies, jobs, leases, executions, receipts, health_observations (D1)
2. **Resource adapters** — ai-lane adapter (discover/health/capabilities/quota/cost/availability/execute/cancel/receipt); job contract never names a provider
3. **Job contract** — job_id, organelle, capability, input, data_class, max_cost, priority, timeout, idempotency_key
4. **Governor** — AUTHORITY → DATA POLICY → CAPABILITY → HEALTH → QUOTA → COST → EFFICIENCY → EXECUTE; every gate recorded in receipt
5. **Deterministic scoring** — weighted: fit .25, reliability .20, latency .15, availability .15, quota .10, cost .10, sovereignty .05; no LLM in placement
6. **Failover** — ranked candidates tried in order; every attempt recorded
7. **Receipts** — every dispatch writes full governor trace + execution results to D1
8. **Health observation** — cron every 5 min probes all lanes; 3 consecutive failures → QUARANTINED; success → AVAILABLE/DEGRADED
9. **Cost rule** — default max_cost 0; paid resources blocked without explicit authority; no paid resources registered in v0
10. **ai-router integration** — lanes are exchange resources; ai-router.js UNTOUCHED (working code, per directive §0); exchange exposes GET /v1/resources for any consumer

## Tests (all live, all evidence in D1 receipts)
- T1 real inference job → done, lane-kilo, receipt rcpt_mux4m52kdzaqdt
- T2 sensitive data → BLOCKED by policy no-secrets-to-keyless (4 rejections)
- T3 unknown capability → BLOCKED (no resource exposes teleportation)
- T4 duplicate idempotency key → deduplicated
- T5 no auth → 401, bad key → 401, no key configured → would be 503
- T6 quarantine lane-kilo → job routed to next candidate (governor exclusion proven)
- T7 execution failover → tried ovh → pollinations → llm7 in order, all recorded
- T8 quota exhaustion → lanes correctly excluded with reason
- T9 all-lanes-down → job failed honestly with full trace, no fake success

## Known defects / limitations (v0)
- Free lanes are flaky by nature (429s, 500s observed live during testing) — the exchange handles this correctly but cannot make them reliable
- Cron probing uses tiny inference calls (costs quota); acceptable at 5-min cadence on 2-30 rpm lanes
- No paid resources registered; external providers (Oracle, NVIDIA, Kaggle, HF, GitHub Actions) need Zackary's accounts — BLOCKED
- assetStorage on cute-city: root cause found (missing BUCKET R2 binding), repair BLOCKED on token scope (R2 403)
- cute-city / email-forwarder / saltspring-slasher / tentacle-ai-router: SOURCE LOST, deployed artifacts preserved
