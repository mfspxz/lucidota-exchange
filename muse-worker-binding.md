# Muse Code as Exchange Worker Population — Binding Design

Date: 2026-10-06. Muse Code 1.4.3 installed at ~/.local/bin/muse.

## Verified capabilities (muse --help, this VM)

- `muse exec` — run one prompt non-interactively (headless). THE worker primitive.
- `muse resume` — resume a previous session (--last or session-ref).
- `muse session-message` — list/send cross-session messages.
- `muse serve` — MSP session host over stdio.
- `muse schema` — export MSP wire schema.
- `--workspace <PATH>` — policy-gated workspace tools rooted at PATH.
  This is the ALLOWED_SURFACE mechanism, built in.
- `--agents <JSON>` — ephemeral agent-definition overlay.
- `--provider meta|local|echo`, `--model`, `--reasoning-effort`.

## Architecture (Zackary's design, confirmed viable)

Muse Code is the worker population, NEVER the governor. The Exchange's
Worker Protocol v1 is already worker-agnostic: a Muse worker is another
worker implementation beside nim-claimant.py. The governor doesn't care
what executes; it cares about the protocol.

```
EXCHANGE GOVERNOR (authority/state/receipts — unchanged)
        │  Worker Protocol v1: JOB → CLAIM → EXECUTION → RECEIPT → ACCOUNTING
        ▼
muse-worker.py (new, mirrors nim-claimant.py)
  1. PICKUP    → POST /v1/claim (worker-scoped auth)
  2. VALIDATE  → delegation fields present, lease live
  3. ISOLATE   → git worktree per attack (Muse's own mechanism)
  4. EXECUTE   → muse exec --workspace <allowed-surface> "<attack prompt>"
  5. CAPTURE   → diff + test output + receipts, no interpretation
  6. REPORT    → POST /v1/complete, one terminal outcome
```

## Bounded attack spec (goes in the job's input field)

```json
{
  "attack_id": "ATTACK-2026-10-06-007",
  "target_invariant": 4,
  "current_state_digest": "sha256:…",
  "allowed_surface": ["experiment/harness.py", "experiment/corpus.py"],
  "frozen_constraints": ["no new primitive without witnessed collapse",
    "master invariant holds", "existing checker is canonical"],
  "success_condition": "reproduction script + checker receipt showing UNSAFE",
  "failure_condition": "no invariant break after bounded attempts",
  "required_receipts": ["repro.py", "checker-receipt.jsonl"],
  "max_scope": "patch-with-tests"
}
```

Workers produce candidate evidence. The Governor merges. No worker rewrites
canonical state directly.

## Long horizon belongs to the Exchange, not Muse

If a Muse worker dies: the Exchange still knows hypothesis, claimed scope,
evidence produced, last receipt, unfinished work, next attack, authority,
state digest, worktree, branch. Another worker resumes via `muse resume
<session-ref>` or a fresh claim. Muse's own event log is a second record,
not the primary one.

## BLOCKER

`muse login` / `muse auth` requires Zackary's Meta developer account.
Install is done; authentication is his step. Nothing worker-side can run
until then.

## Not reinventing

Worktrees, event log/replay, session messaging, headless exec — jacked
from Muse. What Muse doesn't solve (claim≠truth, worker signal≠authority,
derivation≠provenance, the non-collapse invariants) stays LUCIDOTA's
layer, enforced by the Governor and the checker, not by the workers.
