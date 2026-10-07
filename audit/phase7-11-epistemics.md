# GEPPETTO HARD — Agent D (Epistemics): Phases 7–11

Grounded in `~/workspace/exchange/worker.js` (2053 lines, v0.6.0) and live D1 state.
Constitution respected: no Worker Protocol v1 changes proposed.

---

## PHASE 7 — EPISTEMIC STATE: WORLD / KNOWLEDGE / ACTION

**Verdict: the architecture already implements a two-axis separation. It is sufficient. One concrete gap.**

### What exists (proven in code)

Two parallel state axes on `resources`:

| Axis | Field | Meaning | Role |
|------|-------|---------|------|
| Health | `resources.state` (AVAILABLE/DEGRADED/THROTTLED/QUARANTINED/UNKNOWN) | Rolling belief from last-3 `health_observations` (`observe()`, worker.js:862) | KNOWLEDGE about current condition |
| Belief tier | `resources.evidence_state` (DISCOVERED→VERIFIED ladder) | Warranted by recorded evidence (`warrantedTier()`, worker.js:318) | KNOWLEDGE authorized for action |
| Action gate | `EVIDENCE_USABLE = [AVAILABLE, VERIFIED]` | Checked at EVIDENCE gate (worker.js:1076,1432) | ACTION permission |

KNOWLEDGE is derived from evidence (`warrantedTier` counts observations, never asserts).
ACTION requires the tier. The `reliability = 0.5` prior for no-data (worker.js:1115) is
honest — neutral, and recorded in the score breakdown.

### Where it conflates (observed, not theorized)

1. **`warrantedTier` reads `r.state` (health) as input to compute the evidence tier.**
   KNOWLEDGE-about-health feeds KNOWLEDGE-about-capacity. If the health rollup is
   wrong (e.g., poisoned observations — the redteam's synthetic 429 did exactly this),
   the evidence tier inherits the error. Coupling, not separation.
2. **No representation for contradictory evidence.** Alternating ok/fail observations
   just average into DEGRADED (rate < 0.8). The system cannot say KNOWLEDGE=UNRESOLVED;
   it collapses conflict into a weak tier and loses the information that evidence
   *disagrees* vs is merely *thin*.
3. **Past fix proves the failure mode:** the QUOTA gate treated any historical 429 as
   permanent — old KNOWLEDGE mistaken for WORLD. Fixed with 60-min decay (v0.5.1).
   The architecture learns this lesson per-bug, not structurally.

### Where the substrate would help (not forced)

- **One addition earns its keep:** a `CONTESTED` marker (or evidence_log-derived flag)
  when recent observations contradict (e.g., ok-rate between 0.3–0.7 with n≥5).
  Cost: one query in `observe()`. Benefit: distinguishes "weak evidence" from
  "conflicting evidence" — different governor responses (probe more vs back off).
- **Do NOT add a third axis.** The two-axis system plus gate-level policy (COST gate
  already separates "believed good" from "authorized to spend") covers
  REALITY=UNKNOWN / KNOWLEDGE=PROVISIONAL / ACTION=ACCEPTED. A third axis is
  ontology theater at this scale.

**Rating: STRONGLY SUPPORTED** (two-axis works; contested-evidence gap is real but narrow).

---

## PHASE 8 — MINIMUM USEFUL ONTOLOGY

**Verdict: 12 entities earn existence; 1 table should be deleted; 1 could fold. No new primitives.**

### Load-bearing entities (every one is read by executable code paths)

| Entity | Table | Earns existence because |
|--------|-------|------------------------|
| JOB | jobs (54 rows) | intent + governed decision; everything keys off it |
| LEASE | leases (44) | reservation; claim atomicity depends on it |
| CLAIM | claims (13) | worker binding; W1/W2/W3 all operate on it |
| EXECUTION | executions (43) | what actually ran; requested-vs-actual lives here |
| RECEIPT | receipts (72) | the believed evidence; verification endpoint reads it |
| ACCOUNTING_RECORD | accounting (9) | the books; separate by constitutional requirement |
| RESOURCE | resources (25) | capacity identity |
| WORKER | workers (3) | executor identity; auth binds to it |
| OBSERVATION | health_observations (316) | raw evidence; `warrantedTier` and `observe()` consume it |
| EVIDENCE_EVENT | evidence_log (135) | provenance; every tier/health transition logged |
| POLICY | policies (2) | rules; `policy_snapshot` frozen into claims |
| PROVIDER | providers (17) | grouping; provider-down detection acts on it |

### Delete / fold

- **DELETE `discovery_sources` (0 rows).** Nothing writes it, nothing reads it.
  Unearned abstraction. (Phase 13 permission exercised.)
- **COULD FOLD `quotas` (6 rows) into `resources`.** Quota is a measured property
  of a resource, not an independent entity — no code treats a quota as
  addressable apart from its resource. Keep the table or fold; behavior-neutral.
  Not worth a migration today.
- **`resource_capabilities` is a relation (CAPABILITY_OF), not an entity.** Fine as is.

### Earned relations (each is a real JOIN or lookup in code)

- CLAIM **BINDS** Worker→Job (`claims.worker_id`, `claims.job_id`)
- EXECUTION **RECORDS** Claim (`executions.claim_id`)
- RECEIPT **ATTESTS** Execution (`receipts.execution_id`)
- ACCOUNTING_RECORD **SETTLES** Execution (`accounting.execution_id`)
- LEASE **RESERVES** Resource FOR Job
- OBSERVATION **SUPPORTS**/**DISCONFIRMS** Resource (`warrantedTier`, `observe()`)
- EVIDENCE_EVENT **SUPERSEDES** prior tier (`evidence_log` from→to)
- POLICY **GOVERNS** Job (`policy_snapshot` in claims)

### Rejected candidates (do not add)

- **HYPOTHESIS** — nothing in the system hypothesizes. Dead primitive.
- **CREDENTIAL** as entity — `credential_ref` is opaque *by constitutional design*.
  Reifying it would violate invariant #5.
- **PROVENANCE** separate from EVIDENCE_EVENT — `evidence_log` IS provenance.
- **EVENT** separate from EVIDENCE_EVENT — no non-evidence events are logged.

**Rating: PROVEN** (tables + row counts + code reads).

---

## PHASE 9 — NEUROSYMBOLIC BOUNDARY (95/5)

**Verdict: the boundary already exists and is named. The evidence ladder IS the boundary.**

### The boundary, as built

The EFFICIENCY gate (worker.js:1107) states it explicitly: *"Scoring ranks; it never
authorizes."* The weights (`0.20 * 1.0 + 0.15 * reliability + …`) are human judgment
frozen into deterministic code. An LLM may propose different weights; the machinery
applies them; the governor's policy version records which weights decided. That is
the 95/5 split made executable.

### Where generative AI may propose (without authorizing)

| Proposal surface | Current mechanism | Why it's safe |
|------------------|-------------------|---------------|
| Efficiency weights | hardcoded constants | Change = code deploy + policy version bump; never runtime judgment |
| Discovery | `discover-local.py` scans skills → registers DISCOVERED leads | Leads are not usable; the ladder evaluates them |
| Error taxonomy | substring matching in `observe()` (deterministic) | An LLM could *suggest* new classes; they enter as code |
| Degradation diagnosis | none (read-only) | "Why did nim-nvidia degrade?" as a diagnostic query changes nothing |

The pattern is uniform: **AI proposes → proposal enters as DISCOVERED/unevaluated →
deterministic machinery evaluates → governor authorizes.** The evidence ladder is
not just for resources; it's the template for every AI touchpoint.

### Where AI must never act (already enforced)

Gates, leases, accounting, receipts, policy, identity, state transitions — the
10 invariants. No LLM sits in any of these paths today (verified: zero LLM calls
in worker.js; claimants are deterministic scripts).

### The one real risk

The claimant scripts (`nim-claimant.py`) contain the *mechanical* outcome table
(HTTP status → terminal). That table is judgment frozen in the worker, not the
governor. If a future "smart" claimant reinterprets statuses, it silently moves
judgment across the boundary. **Mitigation (no protocol change):** the governor
already re-derives taxonomy via `taxonomyOf(error_class)` and independently
verifies SUCCEEDED output — the worker's classification is treated as evidence,
not authority. This is correct; document it as load-bearing.

**Rating: STRONGLY SUPPORTED.** The boundary is real, named, and tested.

---

## PHASE 10 — METACOGNITION (only what changes action)

**Verdict: five genuine mechanisms. Two decorative absences. No fake confidence anywhere.**

### Real: self-knowledge → changed computation

| Mechanism | Self-knowledge | Changed action | Code |
|-----------|----------------|----------------|------|
| Exponential backoff | "we are being throttled" (consec 429s) | wait 2^(n-1) min, capped 60 | `observe()`, worker.js:883 |
| Quarantine | "3 consecutive failures" | stop routing; require explicit re-verify | `observe()`, worker.js:869 |
| Staleness demotion | "our knowledge is >24h old" | demote AVAILABLE→FREE / VERIFIED→AVAILABLE | `demoteCheck()`, worker.js:347 |
| Retry budget | "we've deferred 8 times" | stop; fail honestly | replan + `completeClaimed`, defer_count |
| Reliability routing | "this lane fails 40% of the time" | rank lower in EFFICIENCY | worker.js:1115 |
| Throttle-until | "don't try before X" | skip candidate | `throttled_until` check, worker.js:1432 |

The `0.5` no-data prior is a prior, not metacognition — it doesn't adapt. Fine.

### Absent (decorative gap, not fake)

- **No uncertainty-driven evidence gathering.** Nothing says "confidence is low →
  probe more." Probing is on fixed cadence (cron) or manual (verify gauntlet).
  The system escalates *away* from uncertainty (backoff, quarantine) but never
  *toward* it (targeted probing). For a capacity exchange this is the right
  default — probing costs real executions — but it's a conscious gap.
- **No confidence values anywhere.** Correct per the mission: no "72% confident"
  decoration exists. The score breakdown records components; nothing pretends
  they're calibrated probabilities.

### What would earn existence

A `probe_priority` derived from (uncertainty × consequence): uncertain + high-value
capability → probe sooner than the staleness cadence. One integer, computed in the
cron, changes probe ordering. Not built; named as the smallest metacognitive upgrade.

**Rating: PROVEN** (mechanisms traced in code; absences verified by grep).

---

## PHASE 11 — TOKEN ECONOMICS (total information movement per verified outcome)

**Verdict: measured. The governor is token-free; the blind spot is provider tokens.**

### Measured cost per verified delegated task (live)

| Segment | Cost | Notes |
|---------|------|-------|
| Governor (submit → gates → lease → delegation) | ~29 D1 statements, 1 HTTP | `submitJob` ~2 + `runJob` ~22 + `delegateExecution` ~5; zero LLM tokens |
| Claim | ~6 D1 statements, 1 HTTP | worker auth + atomic UPDATE + claim/execution rows |
| Worker execution (NIM) | 1 API call | tokens = prompt + completion; **not recorded** |
| Complete | ~25 D1 statements, 1 HTTP | observe + quota + receipt + accounting + claim close |
| Cron amortization | ~1 probe per stale resource per 5 min | O(resources); 25 resources today |
| Hook | 1 curl per 120s | 0 tokens; no agent wakes in v1 |

**Total: ~60 D1 ops + 3 HTTP round-trips + 1 provider call. Zero LLM tokens in governance.**

### Findings

1. **Provider tokens are invisible.** Receipts record `output.content` but not
   `usage` (prompt/completion tokens). The claimant captures usage from NIM
   responses but drops it. For a *free compute* exchange, tokens/quota are the
   actual scarce resource — dollars are $0 by design. **You cannot optimize what
   you don't measure.** This is the single highest-value instrumentation gap.
2. **The 120s poll is cheap but not free.** Each poll forks Python (~10s wall).
   Documented future: event-driven seam. At current scale (25 resources, sparse
   jobs) this is noise; don't optimize yet.
3. **Verification burns real inference.** The delegated gauntlet spends a genuine
   NIM call per verification. Necessary — proof requires execution — not waste.
   But it means "verify everything often" has a token price the books don't show
   (see finding 1).
4. **Failure is cheap, retry is bounded.** Deferred jobs replan via cron (2/run,
   8 max). A poisoned job costs ≤8 wasted gate evaluations, not unbounded tokens.
5. **No context reconstruction cost.** The worker is stateless per request; D1 is
   the memory. There is no "agent turn" overhead in the exchange itself — the
   expensive agent loops live *outside* it (hooks, claimants), which is correct
   layering.

### The optimization target

**Minimum recorded information per verified outcome** = the delegation receipt +
execution record + accounting row + provider `usage`. Today that's ~4 D1 rows and
60 statements; the missing piece is `usage`. Proposed (no protocol change):
claimants include provider `usage` in `/v1/complete`; governor stores it on the
execution row. Then cost-per-verified-outcome becomes computable, and the
EFFICIENCY gate can rank on measured token cost — closing the loop the 95/5
doctrine demands.

**Rating: PROVEN** (statement counts from code; usage gap verified in live receipts).

---

## Cross-phase notes

- **Do not build:** third epistemic axis, HYPOTHESIS/CREDENTIAL/PROVENANCE entities,
  confidence scores, LLM-in-the-gate, poll optimization at current scale.
- **Do build (ranked):**
  1. **MUST:** record provider `usage` on executions (Phase 11 finding 1) — the
     books are blind to the actual scarce resource.
  2. **SHOULD:** `CONTESTED` evidence marker for contradictory observations
     (Phase 7) — one query, distinguishes conflict from weakness.
  3. **COULD:** `probe_priority` from uncertainty × consequence (Phase 10).
  4. **DELETE:** `discovery_sources` table (Phase 8).
- **Constitution untouched.** All proposals build around the frozen interface.
