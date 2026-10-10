# Path to 10/10 — lucidota-exchange security

Scaffold written 2026-10-10 after the external deep source review
(7.4/10 overall: 8.5 architecture, 6.5 security boundary, 8/10 evidence &
testing, 6/10 production confidence) and the hardening campaign that
closed its P1/P1/P2/VERIFY findings (receipt:
`audit/2026-10-10-hardening-receipt.md`, worker.js v0.6.16).

"10/10" here means: every claimed guarantee is enforced in code on the
current revision, tested hostilely on the current revision, and the
deployed artifact is proven to be that revision. No category scores on
intent, history, or architecture alone.

## 1. Threat model

**Assets:** job queue integrity (no forged/pruned work), claim/lease
ownership (one worker per claim), receipt truthfulness (receipts =
what happened), quota/cost accounting ($0 rule), envelope authority
(only the authority key mints ingestable envelopes).

**Adversaries:**
- A1 — compromised or malicious registered worker (valid credentials,
  arbitrary completions: forged outcomes, fake -1 verdicts, replayed
  envelopes, double proposals).
- A2 — network attacker (replay capture→reuse across isolates/routes,
  envelope transplant across claims).
- A3 — buggy-but-honest worker (malformed completions, crash mid-flight).
- A4 — operator error (stale deploy, config drift, schema drift between
  repo DDL and live D1).

**Non-threats (explicit):** Cloudflare infrastructure itself; the
authority key holder (key compromise = total compromise, out of scope —
rotation is Zack's action under the secret freeze).

## 2. What 10/10 means per category

### Architecture — 8.5 → 10
Governor owns authorization/leases/quotas/accounting/receipts; workers
are untrusted execution capacity. 10/10 = the membrane is complete:
every authority-relevant decision is made by the governor from
governor-held state, and no worker assertion is trusted without
independent corroboration or verification. Closed this round:
disproof corroboration now counts distinct eligible workers (A1);
completion validation precedes state transition (A3).

### Security boundary — 6.5 → 10
10/10 = (a) caller authentication on every mutating route; (b) capability
derivation from trusted grants, never from caller assertion; (c) replay
rejection global across isolates; (d) no fail-open path (every error arm
refuses or degrades to a safe state). Closed this round: (c) via D1
seal_registry; (d) extended to registry-unavailable (503). Remaining:
§4.

### Evidence & testing — 8 → 10
10/10 = every security claim has a hostile test running against the
current revision in CI-equivalent conditions, with raw outputs archived
per revision; historical batteries are labeled historical. Closed this
round: `tests/hardening.test.mjs` (18 hostile checks, all red pre-fix /
green post-fix) + revision-bound receipt. Remaining: §4 (re-run older
batteries against v0.6.16; concurrency load tests).

### Production confidence — 6 → 10
10/10 = deployed artifact == reviewed revision (proven, not assumed);
cron/config/schema match the repo; recovery paths exercised against
live D1. Closed this round: cron drift resolved (deployed == wrangler
== `*/30`, read-only API proof). Remaining: deploy v0.6.16 (out of
this campaign's scope) and prove it post-deploy (§4 M1).

## 3. Gaps closed (v0.6.16)

| # | Finding | Fix | Evidence |
|---|---------|-----|----------|
| 1 | P1 disproof corroboration counted proposals, not witnesses | `COUNT(DISTINCT worker_id)` over eligible workers; atomic batch prune; deterministic idempotent prune receipts | T1a–T1h (hostile same-worker-twice ⇒ no prune) |
| 2 | P1 malformed completion stranded claims in `completing` | Validate `worker_outcome` before the flip; pre-existing 15-min stuck recovery confirmed present | T2a–T2c, T3a–T3c |
| 3 | P1/P2 replay per-isolate only | D1 `seal_registry` atomic `INSERT..ON CONFLICT DO NOTHING` arbiter; bounded retention; 503 fail-closed | T4a–T4d (cross-isolate 409; race exactly one 200) |
| 4 | VERIFY cron drift | Read-only API: deployed `*/30` == wrangler.toml; stale comments fixed | schedules endpoint JSON in receipt |

## 4. What remains — milestones with acceptance tests

**M1 — Deploy v0.6.16 and prove deployed==reviewed.**
Acceptance: after deploy, `GET` the live worker's version/health route and
confirm it reports `0.6.16`; re-run `tests/hardening.test.mjs` semantics
against the live endpoint URLs (read-only probes + one synthetic job on
a canary queue); record the deployed SHA-equivalent (wrangler deploy
output) in the next audit receipt. Until then, production confidence
stays capped: the fixes exist in the repo, not in production.

**M2 — Deployed-vs-source drift checks (standing).**
Acceptance: a read-only drift probe (cron: schedules endpoint; source:
a `/.well-known/revision` or health route exposing VERSION; schema:
`PRAGMA table_info` on the seal_registry/expect tables via a read-only
admin route) runs on every deploy and on a weekly read-only cadence, and
writes its verdict to the audit log. Any mismatch (cron ≠ wrangler.toml,
VERSION ≠ repo HEAD, missing table) pages as a finding, not a silent
note. The 2026-10-10 cron check is the template: schedules endpoint
JSON, verbatim, archived.

**M3 — Concurrency adversarial tests.**
Acceptance: multi-isolate replay race and double -1 race executed
against live D1 with real concurrency (not node-interleaving):
N parallel same-seal submits ⇒ exactly 1 accepted; N parallel -1
proposals from one worker ⇒ no prune; from N distinct workers ⇒ prune
exactly once (one prune receipt per dep). Unknowns (1)(2) in the receipt
close here.

**M4 — Re-run historical batteries on the current revision.**
Acceptance: battery v3/v4/v5 + redteam2 suites re-executed against
v0.6.16 with raw outputs archived; each historical claim re-labeled
proven-current or superseded. Historical results never cited as
current-revision proof.

**M5 — Seal-registry flood economics.**
Acceptance: measured cost of a sustained seal-flood (writes/day,
retention-lag under prune cap) against the 100K/day free write quota;
a flood-response runbook (tighten REPLAY_MAX_SEEN, shorten TTL, alert
threshold on 503-full rate). Unknown (4) in the receipt closes here.

**M6 — Claim-state machine audit.**
Acceptance: exhaustive state-transition map for claims
(active→completing→completed/expired + recovery edges) with a test per
edge, including lease-expiry-during-completing and worker-death at each
await point in `completeClaimed`. The A3 class (buggy-but-honest worker)
is closed when every edge is covered.

**M7 — Authority-key hygiene.**
Acceptance: documented rotation procedure for PL_SIG_KEY/EXCHANGE_KEY
under the secret freeze (Zack's action); a key-compromise runbook
(what breaks, what must be re-issued, how in-flight envelopes are
treated). Out of scope for code, in scope for the 10/10 folder.

## 5. Verification strategy (standing)

1. **Revision-bound receipts.** Every campaign writes
   `audit/<date>-<scope>-receipt.md`: base SHA, fix SHAs, exact commands,
   raw test outputs (archived as files, not pasted summaries), findings,
   fixes, remaining unknowns. No receipt = no claim.
2. **Red-before-green.** Hostile tests land failing on the base revision
   and pass only after the fix. A test that never failed proves nothing.
3. **Deployed == reviewed.** M2 drift probes are the only acceptable
   evidence that production matches the repo. Wrangler output, schedules
   JSON, and VERSION responses are archived verbatim.
4. **History stays history.** Red-team reports and old batteries are
   preserved untouched and cited with their revision. Current-revision
   claims require current-revision runs (M4).
5. **D1-budget-first design.** Every new check ships with its read/write
   budget analyzed (the 5M reads/day lesson is permanent): prefer PK
   writes over scans, bounded LIMITs, per-cron-run caps, memory fast
   paths. A security fix that reintroduces the burn is a regression.
6. **Fail-closed by default.** Every new error arm answers: does this
   refuse, or does it degrade open? The answer must be "refuse" with a
   test proving it.

## 6. Scorecard (honest, current)

| Category | Review | After v0.6.16 | Path to 10 |
|----------|--------|---------------|------------|
| Architecture | 8.5 | 8.5 | M6 (state-machine audit) |
| Security boundary | 6.5 | ~8 | M1 + M3 + M5 |
| Evidence & testing | 8 | ~9 | M4 (batteries re-run current) |
| Production confidence | 6 | 6 | M1 + M2 (deploy + drift probes) |

Overall: 7.4 → ~7.9 by this campaign's own scoring discipline.
10/10 is declared only when M1–M7 are all green with archived evidence —
not before, and never by rounding up.
