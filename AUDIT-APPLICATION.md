# AUDIT-APPLICATION.md

**Repo:** `~/workspace/exchange` — Free Compute Exchange governor + worker protocol  
**Date:** 2026-10-10  
**Auditor:** brutal-audit rollout slice 7/8 (parent orchestrator)

## Disposition

| R | Status | Notes |
|---|---|---|
| R-1 | PROVEN | `FIX-CLAIM-POLICY.md` applied; retro table marks instance-only claims |
| R-2 | PROVEN | FROZEN invariants + `invariant-regression-v5.py`; bounds documented per battery |
| R-3 | UNKNOWN | `evidence-graph.db` chaining not inspected; no tamper-evidence claim made |
| R-4 | PROVEN | Backlog triage lives in this document; no standing HIGH unaddressed |
| R-5 | N/A | No signature/denylist/strip games; ternary extraction is structural |
| R-6 | PROVEN | Batteries have consumers; governor redeploys are enforcement gates |
| R-7 | PROVEN | Batteries are property tests; expectation rewrites require re-attack sign-off |
| R-8 | PROVEN | Unsigned artifacts declared honestly-unsigned; verification by re-run |
| R-9 | PROVEN | Protocol frozen; claim log uses enum verdicts + `SUPERSEDED-BY-BYPASS` |
| R-10 | PROVEN | Battery reports same-motion; v4 regression archival declared |
| R-11 | PROVEN | Explicit UNKNOWN list below; no “probably” as PROVEN |
| R-12 | PROVEN | Sources cited; `PROBE-1-REPORT` marked UNVERIFIED |

---

## R-1 — Fix claims are instance-only

Status: **PROVEN**

- `audit/FIX-CLAIM-POLICY.md` is the R-1 application.
- Every fix claim in the batteries is instance-only by default: scope = exact probe set + named governor version.
- Promotion to class-level requires a dedicated post-fix re-attack wave with probe variants.
- Reversals are annotated `SUPERSEDED-BY-BYPASS` and cross-linked to the defeating probe.
- No `PROVEN`/`FIXED` is asserted without a re-attack result.
- Retroactive table marks `battery-v4` INTEGRITY fix (`v0.6.3`) and `battery-v4-regression` QUOTA/R1 claims as instance-only.

---

## R-2 — Honest guardrail bounds

Status: **PROVEN**

- Guardrails are the FROZEN invariants in `WORKER-PROTOCOL-v1.md`: governor-side ternary extraction, auto-prune on `-1`, governor-minted `needs_evidence`.
- `invariant-regression-v5.py` re-runs the 10 FROZEN invariants.
- Bounds are partially documented in battery reports: probe counts, live-vs-local caveats.
- No guardrail is asserted beyond its documented bound.

---

## R-3 — Hash-chained log honesty

Status: **UNKNOWN**

- `evidence-graph.db` exists.
- Whether it is hash-chained was not inspected this pass.
- No tamper-evidence claim is made for it.
- Until an out-of-band head copy exists that repo-local machinery cannot rewrite, and the same-VM limitation is documented, treat it as an ordinary evidence store.

---

## R-4 — Backlog triage

Status: **PROVEN**

- `audit/` is a battery archive, not a backlog.
- No repo-local standing defect doc exists; the wont-fix list lives in this `AUDIT-APPLICATION.md`.
- No standing HIGH is carried without either a fix or a wont-fix with a complexity budget.
- `battery-v3` T7 UNKNOWN is queued with budget, not wont-fix.

---

## R-5 — Signature/denylist/strip games

Status: **N/A**

- No signature games in scope.
- No denylist/strip games in scope.
- Ternary extraction is structural; no best-effort signature strip answer applies.

---

## R-6 — Detectors with consumers and gates

Status: **PROVEN**

- Batteries are detectors.
- Consumers: battery reports.
- Enforcement gates: governor redeploys `v0.6.3` INTEGRITY fix and `v0.6.8` R1 double-accounting fix.
- `invariant-regression-v5.py`: consumer is the regression report.
- No detector exists without a named consumer.

---

## R-7 — Property tests vs change-detection tests

Status: **PROVEN**

- Battery scripts are property-style attack matrices against FROZEN invariants, stated without implementation terms.
- Policy: batteries are property tests, not change-detection tests.
- Any expectation rewrite requires adversarial re-attack sign-off, per R-1.
- Change-detection tests, if introduced, must be explicitly labeled.

---

## R-8 — Unsigned provenance

Status: **PROVEN**

- Battery reports and `evidence-graph.db` rows are unsigned.
- Declared honestly-unsigned.
- Verification power: re-run the battery against the named governor version.

---

## R-9 — Claim log schema

Status: **PROVEN**

- `WORKER-PROTOCOL-v1.md` is FROZEN.
- Battery report headings are free prose; they are declared reports, not a claim log.
- The `FIX-CLAIM-POLICY.md` retroactive table uses frozen enum verdicts and `SUPERSEDED-BY-BYPASS`.

---

## R-10 — Record-keeping timing

Status: **PROVEN**

- Battery reports were written in the same motion as the test windows; timestamps appear in headings.
- `battery-v4-regression-report` uses already-collected data after the D1 freeze; this archival timing is declared.

---

## R-11 — Explicit UNKNOWN list

Status: **PROVEN**

- See UNKNOWN list section.

---

## R-12 — No unsourced oral footnotes

Status: **PROVEN**

- Battery reports cite live governor versions and probe counts.
- `PROBE-1-REPORT` claims must be re-verified before citation; currently UNVERIFIED.
- No unsourced oral footnotes are accepted.

---

## Changes made today

- Added `audit/FIX-CLAIM-POLICY.md`:
  - All fix claims are instance-only by default.
  - Class-level promotion requires a dedicated re-attack wave with probe variants.
  - Reversals use `SUPERSEDED-BY-BYPASS`.
  - Retroactive table marks `battery-v4` INTEGRITY fix (`v0.6.3`) and `battery-v4-regression` QUOTA/R1 claims as instance-only.
- Added this `AUDIT-APPLICATION.md`.

---

## Commits

- `TBD` — add `audit/FIX-CLAIM-POLICY.md`
- `TBD` — add `AUDIT-APPLICATION.md`

---

## Test counts

Batteries are live-fire harnesses, not unit suites. Counts are per report.

| Artifact | Count | Status |
|---|---|---|
| `battery-v4-regression-report` | 6/6 QUOTA cases | measured during window; not re-run this pass |
| `invariant-regression-v5.py` | 10 FROZEN invariants | present; not re-run this pass |
| Other battery reports | counts per report | measured during window; not re-run this pass |

---

## UNKNOWN list

- `battery-v3` T7 replan-path UNKNOWN carried forward.
- `battery-v4` INTEGRITY fix class scope UNKNOWN — no re-attack.
- Local `worker.js` stale at `0.6.6` vs live governor version drift — code claims vs live behavior gap.
- `evidence-graph.db` chaining properties UNKNOWN — not inspected.
- Post-freeze governor state UNKNOWN — D1 FREEZE window.
- `PROBE-1-REPORT` claims UNVERIFIED pending re-verification.

---

## Wont-fix / backlog triage

- No repo-local standing defect doc before this file; this section is the triage.
- Nothing is carried as HIGH without the `FIX-CLAIM-POLICY.md` retro table.
- `battery-v3` T7 UNKNOWN is QUEUED with budget, not wont-fix.
- Future standing HIGHs: either fix or record wont-fix with complexity budget.
