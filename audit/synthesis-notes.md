# Synthesis notes (accumulating)

## Agent D (epistemics, done 22:26 UTC)
- MUST: record provider token `usage` on executions — receipts blind to tokens; tokens are the scarce resource. No protocol change (claimant already captures usage; persist it).
- SHOULD: CONTESTED marker for contradictory observations (currently collapsed into DEGRADED).
- Gap: warrantedTier reads health `state`, coupling WORLD/KNOWLEDGE axes (this is how the synthetic 429 poisoned both).
- Gap: no uncertainty-driven evidence gathering (system escalates away from uncertainty, never toward it).
- DELETE: `discovery_sources` table (0 rows, nothing reads/writes it).
- Confirmed: zero LLM tokens in governance; scoring never authorizes; claimant outcome table correctly treated as evidence.

## Agent A (recon, done 22:27 UTC)
- STRONGLY SUPPORTED: orphan claimed-lease (death between atomic UPDATE and claim INSERT → stuck, unsweepable). Fix queued to E.
- STRONGLY SUPPORTED: partial persistence in completeClaimed (~15 sequential writes, zero transactions).
- STRONGLY SUPPORTED: health-rollup poisoning — hostile worker can QUARANTINE in 3 completions; observe() trusts worker outcomes. Fix queued to E.
- STRONGLY SUPPORTED: accounting asymmetry — inline executions never write accounting rows. Fix queued to E.
- PROVEN live: auth boundaries, trust gate, delegation contract (11 fields, opaque credential_ref), atomic claim, idempotency, forgery/expiry rejection, UNCERTAIN on sweep, cost-assertion ignored, policies immutable, /v1/observe master-key-only.
- DELETE candidates: discovery_sources/runDiscovery (0 rows live), spend_authority/paid path (zero live paid resources — UNKNOWN territory).

## Agent C (state/time, done 22:27 UTC)
- F1 [CRITICAL, fixed+deployed by C]: ISO `T` vs `datetime('now')` space separator — same-day expiries NEVER evaluated as expired. Zombie sweep silently skipped them (recovery delayed up to 24h); replan treated expired leases as live. Fixed with bound ISO param. This is why W5 passed (used 2020 date) while the real 10-min path was broken.
- F2 [MEDIUM]: 16 orphan `active` leases, expires_at=NULL (pre-v0.6 debris). Cleaning up.
- F5 [MEDIUM]: quota_effect={0,0} for quotum-less resources is MISSING disguised as measured-zero.
- PROVEN: UNCERTAIN terminal (single write site, no outgoing transitions); no future-information leakage (immutable spec, frozen policy_snapshot); 2 missing accounting rows correctly left as gap.

## Agent B (attack, done 22:36 UTC) — 21/26 PASS, no constitutional flaw
- HIGH (X8, proven live): hostile worker PARKED a VERIFIED resource with one lied AUTH_ERROR. Worker-asserted error_class → credential disable, no verification. Fix queued to E.
- HIGH (X9, proven live): same vector → THROTTLED + backoff. Fix queued to E.
- MEDIUM (X11): SUCCEEDED without `content` verifies ok — too permissive. Fix queued to E.
- MEDIUM: latency_ms unbounded (negative/huge). Fix queued to E.
- MEDIUM: worker secrets Math.random() → crypto.getRandomValues(). Queued to E.
- INFO: PARKED terminal trap for claimed resources (no auto-recovery).
- HELD: 10-way claim race → 1 winner; 10x replay → 1 accounting; forgery → 403/400; expiry → 409; completion-vs-sweep → ≤1 accounting; zombie → UNCERTAIN; 500KB output bomb handled; trust gate 403.

## Agent E (receipts+build, done 22:47 UTC) — 16 repairs deployed, reattacked, regression 9/9
- Receipt audit: verifier now 13 checks (recomputes output_hash, claim_binding, accounting_bound); per-gate at: timestamps; worker registration logged.
- Orphan claimed-lease sweeper (proven with synthetic orphan). D1 batch for claim+execution INSERTs.
- Health poisoning fixed: 3 worker-only failures cap at DEGRADED; quarantine needs governor corroboration. 1 lied AUTH_ERROR ignored (2 consecutive → PARKED). 1 lied 429 recorded uncorroborated, not acted on.
- Inference SUCCEEDED requires non-empty string content. latency_ms clamped. secrets → crypto.getRandomValues().
- usage_json on executions (proven live). Inline executions write accounting rows.
- Deleted dead variable; named-but-kept discovery_sources (product call) and paid path (frozen unused).
- Constitution intact. v0.6.0 size 139363.
