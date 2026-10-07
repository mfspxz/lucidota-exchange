# Exchange Systems Report — GEPPETTO HARD — 2026-10-06

**Subject:** LUCIDOTA Free Compute Exchange v0.6.0, Worker Protocol v1 (frozen).
**Method:** 5 agents, 14 phases, all against the live system. Receipts before claims.
**Constitution:** intact. No protocol changes. No new adapters.

---

## A. What actually exists

Single Cloudflare Worker (`worker.js`, ~2100 lines) + D1 (`exchange`). One process,
one writer. Tables: jobs, leases, claims, executions, receipts, accounting, workers,
resources, providers, quotas, health_observations, evidence_log, discovery_sources (dead).
The governor runs gates → lease → (inline execute | delegate) → receipt → accounting.
Workers are separate OS processes (VM) or remote, authenticating with scoped credentials.
Cron (5min): probes, auto-promote/demote, provider-down, orphan reaper, zombie sweep, replan.

## B. What is proven (with evidence)

- FAKE_CLAIMANT: full chain, zero governor changes (job_fake-proof-002, rcpt_mux8ilzfrdr4ev).
- W1–W10: 9/9 live. Battery v2 (X1–X28): 21/26 pass, 5 resolved (3 expectation corrections, 1 harness bug, 1 cross-agent interference).
- 10-way claim race → exactly 1 winner. 10× replay → 1 accounting row. Forgery → 403/400/404. Expiry → 409 + replan. Zombie → UNCERTAIN (terminal, 2 live rows).
- Auth boundaries, trust gate (remote→local resource = 403), delegation contract (11 fields, opaque credential_ref), cost-assertion ignored (W6: $99999 → books $0).
- NIM e2e post-hardening: job_final-e2e-001 → done, usage 73/32/105 tokens, receipt 13/13 checks green.
- 13-check verifier: recomputes output_hash, claim_binding, accounting_bound, per-gate timestamps.

## C. What is NOT proven

- Paid path: zero live paid resources — the entire spend_authority/COST-gate paid branch is UNKNOWN territory, never executed.
- `discover-local.py` leads (deepseek/mistral/ai-router/openrouter): registered as DISCOVERED, never verified.
- Multi-region / multi-writer: D1 single-writer assumed everywhere; split-brain untested.
- Clock skew beyond ms-scale: INFORMATIONAL, untested.
- `runDiscovery`: 0 sources live — dead code path, never exercised.

## D. Attack results

**Killed (fixed, deployed, reattacked):**
- CRITICAL: ISO `T` vs `datetime('now')` space separator — same-day expiries never expired. Zombie sweep silently broken for the real 10-min path (W5 passed only because it used a 2020 date). Fixed with bound ISO param.
- HIGH: 1 lied AUTH_ERROR → PARKED a VERIFIED resource. Now: ignored; 2 consecutive → PARKED.
- HIGH: 1 lied RATE_LIMITED → THROTTLED + backoff. Now: recorded uncorroborated, not acted on.
- Worker-only failures cap at DEGRADED; quarantine requires governor corroboration.
- Orphan claimed-lease (death between UPDATE and INSERT): new sweeper catches it (proven with synthetic orphan).
- Inference SUCCEEDED without `content` → now requires non-empty string.
- latency_ms unbounded → clamped [0, 3600000]. Secrets: Math.random → crypto.getRandomValues().
- Accounting asymmetry: inline executions now write accounting rows. usage_json persisted.

**Survived (attacks that held):** 10-way race, replay storms, forgery matrix, expiry, completion-vs-sweep race (≤1 accounting), 500KB output bomb, trust gate, secret scan (zero leaks).

## E. Authority graph

Governor: gates, leases, claims table, receipts, accounting, evidence, policy, worker registry.
Worker: exactly one terminal outcome per claim + raw output. Nothing else.
Crossings: claim (atomic UPDATE = the single concurrency primitive), complete (validated against claim row), observe (worker evidence → governor rollup, now corroborated).
No master-key propagation found. No confused deputy found. `/v1/observe` is master-key-only (proven 401).

## F. State machine

Full FROM→EVENT→GUARD→EFFECT→RECEIPT→NEXT in phase4-6-state.md. Headlines:
UNCERTAIN is terminal — single write site, no outgoing transitions. Claim: active→completed|expired only.
Lease: active→claimed→consumed|released|expired. 15 orphan NULL-expiry leases expired (cleanup).
Residual: completeClaimed has ~15 sequential writes, no transaction (documented; sweeper covers the worst case).

## G. Temporal model

No future-information leakage: spec immutable, policy_snapshot frozen at claim, delegation trace preserved, per-gate `at:` timestamps added. Promotion provenance still weak (F8: promotions don't cite warranting observations) — labeled, not fixed (frozen vocabulary).

## H. Epistemic model

Two-axis system (health `state` × `evidence_state`, EVIDENCE_USABLE gates action) already separates WORLD/KNOWLEDGE/ACTION — sufficient, no third axis. Gap: warrantedTier reads health state, coupling the axes (this is how the synthetic 429 poisoned both). CONTESTED marker for contradictory observations: SHOULD, not implemented (frozen vocabulary — Zackary's call).

## I. Graph model

12 earned entities (all with row counts + code reads). 8 earned relations mapped to real JOINs.
DELETE: `discovery_sources` (0 rows, nothing reads it). Rejected: HYPOTHESIS, CREDENTIAL-as-entity (violates invariant #5), separate PROVENANCE/EVENT.

## J. AI boundary

Zero LLM tokens in governance (verified). "Scoring ranks; it never authorizes" — the evidence ladder is the template for every AI touchpoint: AI proposes → DISCOVERED → machinery evaluates → governor authorizes. Claimant's outcome table is frozen judgment treated as evidence; governor re-derives taxonomy + verifies independently.

## K. Metacognition

Five genuine mechanisms (backoff, quarantine, staleness demotion, retry budget, reliability routing — all traced, all change action). No fake confidence. Gap: no uncertainty-driven evidence gathering (escalates away from uncertainty, never toward it).

## L. Token economics

~60 D1 statements + 3 HTTP + 1 provider call per verified task; zero governance LLM tokens. Blind spot fixed: provider `usage` now persisted — tokens are the scarce resource and the books were blind to them.

## M. Receipt audit

5 "trust me" gaps closed: output_hash recomputed, claim_binding verified, accounting_bound verified, per-gate timestamps, worker registration logged. 13/13 live.

## N. Security findings

- CRITICAL: ISO timestamp bug (fixed).
- HIGH: health-rollup poisoning ×2 (fixed).
- MEDIUM: SUCCEEDED-without-content (fixed), unbounded latency_ms (fixed), Math.random secrets (fixed), no per-worker claim rate limits (open — bounded by 10-min lease expiry).
- LOW: orphan leases (cleaned), quota_effect={0,0} mislabeled (open, frozen vocabulary), cost_basis overclaims (open).
- INFORMATIONAL: clock-span skew, PARKED terminal trap for claimed resources.

## O. Improvements

- MUST (done): usage persistence, ISO fix, poisoning fix, orphan sweeper, accounting symmetry.
- SHOULD: CONTESTED marker, per-worker claim rate limits, promotion provenance (need Zackary's call on vocabulary).
- COULD: uncertainty×consequence probe priority.
- DO NOT BUILD: giant ontology, third epistemic axis, full transaction rewrite of completeClaimed, new privileged adapters.

## P. Code changes

All in `worker.js` (v0.6.0, size 139363, deployed) + `nim-claimant.py` (usage capture) + `worker-battery.py` (harness). 16 repairs, each reattacked.

## Q. Regression proof

W1–W10: 9/9 PASS on fixed code. Battery v2: 21/26 (5 resolved). NIM e2e: done, 13/13.

## R. Final verdict

**Could another engineer add a completely new worker without modifying governor authority logic?**
Yes — proven twice (FAKE_CLAIMANT, battery workers). Register worker, implement 5 terminals, claim/complete. Zero governor changes.

**General execution substrate or accumulating abstractions?**
Substrate. The deleted things (dead variable, and the named-but-spared discovery_sources) are fewer than the proven invariants. Every abstraction added in this pass is load-bearing: claims table, accounting table, usage persistence, corroboration rules. Nothing decorative survived Phase 13.

**Smallest next change with largest capability gain, constitution intact?**
Per-worker claim rate limits + CONTESTED marker. Closes the last known DoS vector (claim-and-abandon griefing) and the last epistemic gap (conflict-vs-weakness). Both are governor-side, zero protocol change.

---

*Phase reports: audit/phase1-2-map.md, audit/phase3-attacks.md, audit/worker-battery-v2.py, audit/phase4-6-state.md, audit/phase7-11-epistemics.md, audit/phase12-14-final.md.*
