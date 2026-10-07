# Multi-Model Voting on the Exchange — DESIGN

**Status:** design + working prototype (`vote.py`). Live-tested 2026-10-07.
**Parent docs:** `~/workspace/adhkey/MUSE-EXCHANGE-DESIGN.md`,
`~/workspace/exchange/WORKER-PROTOCOL-v1.md` (FROZEN — amendments flagged, not assumed).

## 1. Thesis

Voting is a **governor composition pattern**, not a worker behavior.
The worker never knows it is voting. Each vote is one ordinary
delegated job — JOB → CLAIM → EXECUTION → RECEIPT → ACCOUNTING —
and the quorum tally is a governor-side reduction over N receipts.

This preserves all 10 frozen worker invariants by construction:
no new worker capability is introduced, so there is nothing new
for a worker to abuse. A ballot is the governor asking the same
question of N independent capacities and believing the quorum,
not any single model.

## 2. Measured voter set (2026-10-07, NIM account, $0)

Probed live against `integrate.api.nvidia.com`. The catalog lists 80
models; per-account reality is 4:

| Model | Vendor | Latency | Notes |
|---|---|---|---|
| `openai/gpt-oss-20b` | OpenAI | ~1s | reasoning model; answer in `reasoning_content` |
| `z-ai/glm-5.3-flash` | Zhipu | fast | standard `content` |
| `z-ai/glm-5.3` | Zhipu | slower | same vendor as flash — use at most one per ballot |
| `nvidia/nemotron-3.5-lightning-30b-a3b` | NVIDIA | fast | standard `content` |

Everything else: 404/410 (not deployed for this account) or hang
(`deepseek-ai/deepseek-v4.1-flash` — timeout, matches prior findings).

**Default ballot (3 voters, 3 vendors):**
`openai/gpt-oss-20b`, `z-ai/glm-5.3-flash`,
`nvidia/nemotron-3.5-lightning-30b-a3b`.

Vendor diversity is deliberate (see §9). The voter set is
re-probed, never assumed — `vote.py --probe` re-runs the liveness
check before any ballot that needs it.

## 3. Ballot types

### 3a. Verdict ballot (ternary)

The models vote on a proposition: `AUTHORIZED` (+1), `UNKNOWN` (0),
or `DENIED` (−1). This is the epistemic primitive — it composes
directly with `luci-verdict-core`'s ternary verdicts and the
Unknown→Deferred evidence workflow.

Prompt (identical for every voter, temperature 0):

```
<proposition>

Evaluate the proposition above. Think step by step, then give your
final verdict on its own line at the very end, exactly one word:
AUTHORIZED or UNKNOWN or DENIED.

Final verdict:
```

Vote extraction is mechanical: regex `\b(AUTHORIZED|UNKNOWN|DENIED)\b`
over the last 300 characters of output. No match → vote INVALID
(recorded, counts as non-vote, never silently dropped).

### 3b. Answer ballot (exact-match cluster)

Free-text question, one short answer. Prompt ends with:

```
Answer with the final answer only, no explanation:
```

Extraction: normalize (strip, lowercase, collapse whitespace, strip
trailing punctuation), then cluster by normalized equality. The
largest cluster wins iff it holds a strict majority (> n/2) of valid
votes. Otherwise INCONCLUSIVE.

Semantic-equivalence judging by another LLM is explicitly rejected:
the tally must be deterministic and re-computable from the receipts
alone. If answers need semantic comparison, that is a separate
governed job, not part of the tally.

## 4. Vote semantics

- **Majority wins.** Strict >50% of *valid* votes.
- **Tie or no majority → INCONCLUSIVE.** The ballot verdict is
  `UNKNOWN` — never fail-open (counting a tie as authorized) and
  never fail-closed-by-default (counting it as denied). An
  inconclusive ballot routes to the existing Unknown→Deferred
  evidence-gathering workflow, exactly like any other epistemic gap.
- **Quorum.** Default: ⌊n/2⌋+1 valid votes (2-of-3). A ballot that
  cannot reach quorum by deadline is `EXPIRED`, not failed — the
  underlying jobs follow the normal replan path.
- **Dissent is evidence.** Minority votes are kept on the ballot
  receipt in full (model_identity, normalized vote, latency,
  receipt_id). A 2–1 vote records all three; the tally never
  discards the loser.
- **Weights.** Start unweighted (1.0 each). Weighted voting is
  designed but gated: weights derive only from *measured*
  per-model reliability (VERIFY-gate pass rate per `model_identity`
  from the receipt history), never from prior belief about a
  vendor. Until that data exists, weighting is recorded as
  `weight: 1.0, basis: "unweighted — insufficient reliability data"`.

## 5. Disagreement handling

| Situation | Ballot result | Downstream |
|---|---|---|
| Clear majority | winning verdict | receipted as decided |
| Tie / no majority | INCONCLUSIVE → UNKNOWN | evidence-gathering subtask (same as any Unknown) |
| Below quorum at deadline | EXPIRED | jobs replanned per protocol; ballot may be re-run |
| A voter FAILED_RETRYABLE | vote pending | normal replan; ballot waits for quorum or deadline |
| A voter FAILED_PERMANENT | vote void | ballot proceeds on remaining voters |
| A voter returns unparseable output | vote INVALID | recorded; counts toward neither side |

A model that fails is not punished and not retried specially —
it is just a missing vote. The protocol's existing failure
taxonomy already covers every worker outcome; the ballot only
counts *valid votes*.

## 6. Cost accounting

Each vote is a separate execution → a separate accounting row in
the governor's books (measured cost, never worker-asserted). The
ballot receipt references all N `job_id`s and their accounting
rows; `ballot_total_cost = Σ vote costs`. `cost_basis` is recorded
per vote (all $0 on the NIM free tier today; the schema carries
the field so a paid voter can join later without redesign).

A ballot is therefore auditable as: *N independent executions,
each with its own receipt and cost, reduced by a deterministic
tally into one verdict.*

## 7. Idempotency and replay

`idempotency_key = ballot_{ballot_id}_{model_slug}` per vote.
Replaying a ballot submission dedupes per the existing protocol —
no duplicate executions, no double accounting. Re-running a
*decided* ballot with the same `ballot_id` returns the recorded
tally (the receipt is the system of record).

## 8. Deadlines

Default 600s from first submission. The ballot polls job receipts
until quorum-decided or deadline. After deadline with quorum
unreached → EXPIRED. Late votes that arrive after expiry are still
receipted as executions (their receipts exist) but do not alter
the ballot — the ballot receipt records `late_votes: [...]`
honestly.

## 9. Anti-collusion and voter independence

Votes are only as independent as the voters. Known correlations:

- Same-vendor models share training lineage (the two Zhipu GLMs).
  Default ballots take at most one voter per vendor.
- All voters currently share one provider (NVIDIA NIM) — a
  provider-level fault correlates all votes. Recorded as a
  standing caveat on every ballot receipt
  (`correlation_domain: ["nvidia-nim"]`).
- Future voters from other lanes (ai-router keyless, local
  models) widen the correlation domain; the ballot schema
  already carries per-vote `provider` for this.

Weighting by measured reliability (§4) must discount correlated
voters, not just reward accurate ones — two models that always
agree add one vote's worth of evidence, not two. The discount
factor is future work, gated on disagreement data.

## 10. Protocol amendments (REQUIRE ZACKARY'S APPROVAL)

Worker Protocol v1 is frozen. This design needs **no worker
changes** — but the governor-side ballot object is new. Flagged
as explicit amendments:

- **A1 — `/v1/ballots` endpoints** (submit / get / tally).
  New objects only; no existing endpoint changes shape or behavior.
- **A2 — receipt `ballot_id`.** Optional field on receipts linking
  N vote receipts to one ballot. Additive; existing receipts
  unaffected.
- **A3 — quorum policy object.** Governor-side config: default
  voters, quorum rule, deadline, tie policy. No worker visibility.

Until A1–A3 are approved, ballots live **outside** the governor:
`vote.py` submits ordinary jobs and tallies client-side, writing
the ballot receipt as a local JSON artifact + ledger entry. The
tally logic is identical; only the storage location differs.
This is the honest staging: the mechanism is proven before the
protocol is extended.

## 11. What voting does NOT protect against

- A proposition the prompt itself smuggles (garbage in, voted garbage out).
- All voters sharing a blind spot (correlated error — §9).
- A 2–1 majority that is confidently wrong. Voting measures
  *agreement*, not *truth*. The ballot receipt records votes;
  `luci-verdict-core` still decides what counts as evidence.
- Prompt-injection via the proposition text. The ballot submitter
  is trusted (governor-side); propositions are not laundered
  through untrusted input without a gate.

## 12. Prototype

`~/workspace/exchange/voting/vote.py`:

```
vote.py verdict "Proposition text" [--voters m1,m2,m3] [--deadline 600]
vote.py answer  "Question text"    [--voters ...] [--deadline 600]
vote.py --probe                   # re-check which NIM models are live
```

Submits one Exchange job per voter (`input.model` selects the
model; `input.temperature: 0`; idempotency keys per §7), waits for
the claimant workers to execute, extracts votes mechanically,
tallies per §4, writes
`~/workspace/exchange/voting/ballots/{ballot_id}.json`, and prints
the verdict. Exit 0 on decided, 2 on INCONCLUSIVE, 3 on EXPIRED.

Claimant-side enabler (non-protocol, Zackary's own worker code):
`nim-claimant.py` now honors `input.temperature` (default 0.7
unchanged) so ballots can request deterministic votes.

## 13. First live results (2026-10-07)

**Ballot 1 — answer** (`bal_ec1ecb96b07aabca_43053`): "What is 17
multiplied by 23?" → verdict **391** (correct), 2/3 majority
(gpt-oss-20b ✓, nemotron-3.5-lightning ✓). glm-5.3-flash vote lost
to a NIM read timeout → FAILED_RETRYABLE → job deferred for
replan per the normal protocol. The ballot decided on quorum
without it — exactly the designed behavior.

**Ballot 2 — verdict** (`bal_3404ac18f2e2a9c5_43513`): "17 multiplied
by 23 equals 391." → verdict **AUTHORIZED**, 2/3 majority,
mechanical regex extraction from the output tail. glm-5.3-flash
timed out again (2/2 under Exchange load; direct probes succeed —
measured reliability signal, recorded not judged).

Receipts: `~/workspace/exchange/voting/ballots/*.json`.
