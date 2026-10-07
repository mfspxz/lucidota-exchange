# IPB Leaderboard + SOTA Synthesis — LANE G (2026-10-07)

## SOTA scan: swarm orchestration, epistemic/temporal graphs, multi-agent verification

### Swarm orchestration (2026)
- **arXiv 2605.02801** (May 2026): definitive RL-for-orchestration survey, 84 papers.
  Five sub-decisions: spawn timing, delegation, communication, aggregation,
  **stopping**. The stopping sub-decision has **zero RL training methods** in
  the literature — every framework uses static policies. Our ternary
  DEFER/terminal verdicts are a principled answer to an open problem, but we
  have not measured stopping quality against anything.
- **SWARM+** (2026): 990 distributed agents, ~1s per-job selection, 97–98%
  latency reduction vs prior SWARM, >97% job completion under failures.
  Scale benchmark we should beat or match on completion-under-failure.
- **MASBENCH** (ICML 2026, arXiv:2601.14652): controlled benchmark varying
  Depth/Horizon/Breadth/Parallel/Robustness; MAS-Orchestra RL-selects whole
  multi-agent structures. Lesson: measure dependency structure + a
  single-agent baseline; do not treat task difficulty as evidence for fan-out.
- **Google AX** (Sept 2026, HN 649pts/296 comments): declarative orchestrator;
  underneath, Agent Substrate with gVisor sandboxing, GCS-backed pod
  snapshots, sub-second suspend/resume ("Actor Teleport"), bypassing the K8s
  control plane on the hot path. Industry is converging on: orchestration is
  a distributed-systems scheduling problem, not a prompting problem.
- **"A Million Agents Is a Distributed Systems Problem"** (HN, Sept 2026):
  Google Research 180-config study — agents help parallel work +80.9%, hurt
  sequential work −39% to −70%. An orchestrator cut error amplification
  **17.2× → 4.4×**. Silo-Bench (ACL 2026): 2–100 agents, hardest tasks hit
  **zero success at 50 agents**. Durable thing = the state, not the agent.
  "Schedule agents like processes and recover them like nodes."

### Multi-agent verification (2026)
- **"From Spark to Fire"** (arXiv 2603.04474, Google DeepMind): unstructured
  multi-agent networks amplify errors **up to 10.31×**; star topologies suffer
  100% system failure from hub corruption; mesh contaminates within 3 rounds.
  Mitigation: genealogy-based governance with **tri-state screening
  (Green/Red/Yellow)** — 89–94% defense vs 32% baseline. This is independent
  convergence on our ternary logic; we should cite it and steal the
  risk-stratified verification idea (prioritize high-influence agents).
- **LLM Multi-Agent Debate Fails** (May 2026): homogeneous 7–8B debate teams
  perform *worse* than isolated self-correction at 2.1–3.4× the tokens.
  Sycophantic conformity up to 85.5%, consensus collapse. Direct support for
  Zackary's "no consensus soup" directive and for cross-model (not
  same-model) voting.
- **Refute-or-Promote** (arXiv 2604.19049): "consensus among AI agents does
  not equal correctness" — 80+ agents unanimously endorsed a non-existent
  vulnerability. Answer: adversarial agents tasked with disproving findings +
  Cross-Model Critic + mandatory empirical validation. We have adversarial
  batteries; we do NOT yet have a mandated cross-model critic in the
  pipeline. The voting lane is the right home for it.
- **BlockAgents**: blockchain + proof-of-trust for Byzantine fault tolerance
  in swarms. Overkill for us, but the threat model (unreliable swarm members)
  is one we should name explicitly.

### Temporal / epistemic graphs (2026)
- **Graphiti/Zep**: bi-temporal TKG (valid time + transaction time), edge
  invalidation never deletes. One homelab retired it Aug 2026 after it
  "hit the Anthropic API hard enough to lock the billing account" — LLM
  extraction cost is the known failure mode. Our edge-writing is
  deterministic (no LLM extraction), which dodges their cost blowup.
- **SimuHome TKG** (ICLR 2026): TKG agent vs ReAct baseline — same 83%
  success, **~40% fewer tool calls**. Empirical support for the IPB thesis:
  structured memory cuts calls, not just tokens.
- **Spreading activation** (neural-memory, surreal-memory): typed synapses
  (CAUSED_BY, CONTRADICTS, …), O(n) local traversal, no embeddings needed.
  surreal-memory is built on SurrealDB — directly comparable to our
  projection; worth a benchmark shootout.
- **NEXO claim graph**: contradictory beliefs surface as *dissonance events*
  ("My memory says X, you're now saying Y") rather than silent overrides.
  We have verdict −1 but no first-class dissonance edge/UX. Gap.

### What we are missing (honest)
1. **Stopping quality**: literature has no trained stopping policies; our
   ternary defer is principled but unmeasured. Need a stopping benchmark.
2. **Error amplification factor**: DeepMind measured 10.31×; Google measured
   17.2×→4.4× with an orchestrator. We claim ternary pruning bounds it but
   have never measured ours. Add amplification to the leaderboard.
3. **Cross-model critic**: mandated adversarial reviewer in the pipeline
   (Refute-or-Promote). Voting lane should include it.
4. **Stigmergy**: SwarmWorld (MIT) showed agents differentiating via shared
   environment without direct communication. Our graph IS a stigmergic
   medium — we have not framed or measured it as such.
5. **Dissonance events**: first-class contradiction surfacing (NEXO). Our
   −1 verdicts are silent unless queried.
6. **Retrieval shootout**: spreading activation (surreal-memory) vs our
   CAS projection — unmeasured.

## IPB leaderboard

*Score = verified / (bytes + 1000 × latency_s). Higher is better.*

### Fleet baseline (2026-10-06, 21 jobs in window)
- Fleet: 266.4 KB in+out, 845 s latency, score **1.879e-05**
- Mean: **12.7 KB/decision, 40 s/decision**
- Top: tiny probe jobs (~2.5e-04). Bottom: deep redteam analyses r1/r2/r3 (~6–8e-06)
- Law: latency dominates for reasoning-model jobs (r2: 75% of cost is latency)

### Lane-G refresh (2026-10-07, 03:40Z)
- 19/19 verified in window. Fleet: **20.8 KB in+out, 226 s latency,
  score 7.671e-05**. Mean: **1.1 KB/decision, 12 s/decision**.
- Window dominated by battery-v3 micro-probes (top: ~1.7e-04) and lane-E
  tricycle jobs (bottom: ~2e-05).
- **IPB script bug fixed by LANE G**: `get_all_receipts()` hung forever
  because the endpoint ignores `offset` — loop now breaks when a page adds
  no new receipt IDs.

| Job family | n | Mean score | Verdict |
|---|---|---|---|
| Battery v3 micro-probes | 12 | ~1.4e-04 | top of board; tiny + fast |
| F12 gpt-oss-20b (LANE G) | 1 | 1.433e-04 | **ALIVE** — executed, rank #6 |
| Lane-E tricycle jobs | 3 | ~2.7e-05 | substantive work, honestly priced |
| F12 gpt-oss-120b | 1 | — | **DEAD** — 410 end-of-life |
| F12 deepseek-v3.1 | 1 | — | **DEAD** — 404 not served |
| F12 qwen3-32b | 1 | — | resubmitted after 429; pending |
| F9 verify-endpoint | 1 | — | delegates clean below cap (v0.6.2 ok) |
| F10 INTEGRITY hunt | 3 | — | running (3 reps, 60 s spacing) |

### Model reality for the voting lane (measured, not catalog)
- `openai/gpt-oss-20b`: ALIVE (only proven model on this account)
- `openai/gpt-oss-120b`: 410 EOL — dead
- `deepseek-ai/deepseek-v3.1`: 404 — dead (this ID)
- `qwen/qwen3-32b`: pending execution
- Implication: cross-model voting cannot run on NIM alone today — one live
  model. The voting lane needs a second live model (OpenRouter?) or must
  vote across heterogeneous lanes, not homogeneous NIM models. This
  independently confirms the May 2026 debate-failure finding: homogeneous
  same-model "voting" is theater.

### Underperformers (honest)
- Deep redteam analyses (r1/r2/r3): ~6–8e-06, ~3× below fleet mean. Cause:
  latency (116 s × 1000 = 116 kB-equiv vs 39 kB actual bytes). These are
  high-value outputs (they found live bugs) — the metric prices their
  *cost*, not their *worth*. Do not optimize them away; price them honestly.
- Any job that 429s repeatedly: pure waste — the probe harness now halts
  instead of spinning, which is the correct behavior.

## Probe follow-up results (LANE G campaign)
- **F9**: verify endpoint delegates cleanly below cap post-v0.6.2 (no 429 at
  healthy depth). The 429 path itself was proven by battery v3 T1 pre-fix.
- **F12**: model reality — gpt-oss-20b ALIVE (executed, IPB rank #6);
  gpt-oss-120b DEAD (410 end-of-life); deepseek-v3.1 DEAD (404); qwen3-32b
  resubmitted after honoring a 429 (backpressure working, not a model
  verdict). Full table in the leaderboard section above.
- **F10**: INTEGRITY state-variable hunt — **complete, 3/3 PASS**
  (03:39/03:41/03:42Z). The odd-classifier boundary case (9925 chars /
  max_tokens 1500) that FAILED 3/3 at 02:15–02:35Z now PASSES 3/3.
  Non-stationarity confirmed across regimes; the state variable behind the
  flip is still unknown (queue depth and attestation freshness remain
  candidates). The gate did not flap within the 3-minute window — the
  regime is stable, just different from 02:35Z.
