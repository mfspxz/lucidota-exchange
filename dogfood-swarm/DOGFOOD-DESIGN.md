# Dogfood Lane: Swarm Tasks as Exchange Jobs (v2 — graph-native)

**Status:** PROTOTYPE (minimal loop building 2026-10-07).
**Supersedes:** v1 design (daemon evidence-queue, LLM-judged verdicts).
**Architecture correction (Zackary, 2026-10-07):** the daemon direction is
dead. Constraints below are non-negotiable.

## Non-negotiable constraints

1. **NO daemon watching evidence-queue.** Evidence subtasks are graph nodes
   (task records with `needs_evidence` edges), not JSON files. Workers pull
   them via the Exchange `/v1/claim` mechanism — **the Exchange IS the
   queue.** Nothing polls a directory; nothing watches anything.
2. **NO LLMs as determinism theater.** Ternary verdicts come from
   deterministic graph queries and deterministic execution checks — never
   from asking a model "is this proven?". **LLMs generate claims; the graph
   decides.** The model is the subject under test, never the judge.
3. **NVIDIA NIM is the validator.** Verification = EXECUTE the claim's
   probe against NIM (run it, capture output) and apply a deterministic
   check (exact/contains/hash comparison) to the output. The verdict is a
   pure function of (actual output, expected output). No model opinion
   anywhere in the verdict path.
4. **Everything is a graph, everything durable** (SurrealDB schema +
   CAS + receipts), fast (embedded), ruthless (ternary pruning).

## The mapping (v2)

| Swarm concept | Exchange/graph concept | Notes |
|---|---|---|
| Claim | `lifelog_edge` (edge_kind `claims`, verdict 0) | LLM-generated; untrusted until verified |
| Evidence subtask | Graph task node + `needs_evidence` edge (claim → task) | Durable node, not a file; status transitions in-graph |
| Task queue | The Exchange itself (`/v1/job` → `/v1/claim`) | No daemon. The governor's deferred-delegation queue IS the work queue |
| Worker agent | Claimant process (`swarm-claimant.py`, worker:swarm-claimant-01) | Boring worker; fans out to validator subprocess |
| Validation execution | NIM runs the claim's probe (`swarm-worker.py`) | Model executes; deterministic code judges |
| Ternary verdict | Pure function: `check(output, expect)` → {+1, 0, -1} | Exact/contains/hash; network/parse failure → 0 |
| Verdict record | `lifelog_edge` (edge_kind `verifies`, verdict ±1/0, claimant, receipt_id) | Bound to the Exchange receipt; walkable backward |
| Prune / defer / promote | -1 → prune branch; 0 → task stays open (re-queue); +1 → promote claim edge | Graph queries drive routing, not model votes |

## Task type: `execute-verify` (v1)

The ONLY task type in the minimal loop.

```json
{
  "task_type": "execute-verify",
  "graph_task_id": "task:<id>",
  "claim": "human-readable proposition under test",
  "claim_id": "claim:<id>",
  "execution": {
    "model": "openai/gpt-oss-20b",
    "prompt": "Reply with exactly: ok",
    "max_tokens": 64
  },
  "expect": {
    "match": "exact",
    "value": "ok"
  }
}
```

`match` ∈ `exact` | `contains` | `hash-sha256`. The worker:

1. Sends `execution.prompt` to NIM (the model **executes**, nothing more).
2. Extracts raw output text (content or reasoning_content — mechanical).
3. Applies the deterministic check — **no model is asked anything**:
   - `exact`: `normalize(output) == normalize(expect.value)` → +1 else -1
   - `contains`: `expect.value in output` → +1 else -1
   - `hash-sha256`: `sha256(output) == expect.value` → +1 else -1
4. NIM/network failure, empty output, unparseable → 0 (unknown, honest —
   the execution didn't produce evidence, not a verdict).

`normalize` = strip + collapse whitespace. Deterministic, documented,
byte-stable. No Unicode games: the check operates on raw UTF-8 bytes.

## Why this is not determinism theater

The failure mode Zackary killed: asking an LLM "is claim X proven?" and
treating its answer as a verdict. That is a stochastic parrot wearing a
judge's robe.

In this design the LLM never emits a verdict. It emits *behavior* (text
for a prompt). A deterministic function maps (behavior, expectation) to
{+1, 0, -1}. The same bytes always produce the same verdict. The model
could be replaced by a shell script returning canned text and the verdict
logic would be unchanged — that is the test that it is not theater.

## Graph-native evidence tasks

An evidence subtask is two durable records (JSONL, exact DDL schema;
SurrealDB-ingestible):

```json
{"record": "task:ev-001", "kind": "evidence-task",
 "claim_ref": "claim:c-001", "status": "pending",
 "created_at": "2026-10-07T03:20:00Z", "created_by": "lucidota"}
{"table": "lifelog_edge", "in": "claim:c-001", "out": "task:ev-001",
 "edge_kind": "needs_evidence", "claimant": "lucidota",
 "source": "system", "necessity": "required", "verdict": 0,
 "receipt_id": "rcpt-dogfood-001", "hash": "cas:..."}
```

The Exchange job spec carries `graph_task_id`. When the claimant
completes, it appends:

```json
{"table": "lifelog_edge", "in": "task:ev-001", "out": "claim:c-001",
 "edge_kind": "verifies", "claimant": "worker:swarm-claimant-01",
 "source": "system", "necessity": "required", "verdict": 1,
 "receipt_id": "<exchange receipt id>", "hash": "cas:<output sha256>"}
```

Task status transitions pending → verified | disproven | still-unknown
are recorded as further edges, never by mutating the node. The graph is
append-only; the Exchange is the queue; nothing watches a directory.

## The full loop (minimal)

```
GRAPH:  claims(claim:c-001, verdict=0, claimant=llm:X)
  → needs_evidence → task:ev-001 (pending)
EXCHANGE: POST /v1/job {capability:cognitive, input:{task_type:execute-verify,
           graph_task_id:task:ev-001, ...}}
  → governor gates → lease → deferred (the queue)
WORKER: swarm-claimant-01: /v1/claim → delegation
  → subprocess swarm-worker.py: NIM executes probe
  → deterministic check → {ternary_verdict:+1, output_hash}
  → /v1/complete {worker_outcome:SUCCEEDED, output:{ternary_verdict:+1,...}}
EXCHANGE: receipt (D1) binds claim_id + worker_outcome + output
GRAPH:  verifies(task:ev-001 → claim:c-001, verdict=+1,
          claimant=worker:swarm-claimant-01, receipt_id=<receipt>)
```

## Protocol amendments: NONE required

The frozen Worker Protocol v1 is untouched:

- New capability (`cognitive`) — data, not protocol.
- Resource registered via API — protocol-supported path.
- Claimant fanning out to a subprocess that calls NIM — the protocol
  governs claim→complete; what the claimant does between is its own
  execution authority (same as nim-claimant holding the vault key).
- Ternary verdict in worker output — output schema is worker-defined.
- Evidence tasks as graph nodes pulled via /v1/claim — this is just
  jobs; the graph is the coordinator's own state.

Flagged for future (needs Zackary's approval, NOT implemented):

1. Governor-side ternary extraction into a first-class receipt field.
2. Auto-prune: governor marks dependent deferred jobs when a claim
   edge goes -1.
3. `needs_evidence` edges minted by the governor itself on Unknown
   verdicts (currently the coordinator mints them).

## Files

- `swarm-claimant.py` — boring claimant (PICKUP→VALIDATE→EXECUTE→CAPTURE→REPORT)
- `swarm-worker.py` — validator: NIM executes probe, deterministic check → verdict
- `graph-drops/` — task nodes + lifelog_edge JSONL (exact DDL schema)
- `RUN-001.md` — first proven loop report
