# exchange — organelle mapping decisions

Free Compute Exchange repo (`~/workspace/exchange`): mint protocol
(claimant/submit/worker), governor gates, D1 mirror, evidence graph,
benchmark harness. Prior wave-5 pass stamped all 10 units with one shared
codon `ex` (host-registry collision); fixed here with distinct codons.

## Capability units found

| Unit | Script | Organelle op | Decision |
|---|---|---|---|
| exchange-ballot-cost-rollup (`eb`) | `ballot-cost-rollup.py` | `rollup` | **Bespoke.** Ballot cost accounting rollup (read-only): vote costs → ballot_total_cost, cost basis per vote, ballot receipt references job_ids. Exchange accounting; no ref fits. |
| exchange-deploy-governor (`eo`) | `deploy-governor.py` | `govern` | **Bespoke.** Deploys the Exchange governor `worker.js` via the Cloudflare Workers API (upload ES module + D1 binding, set cron triggers). ExternalEffect (deployment). |
| exchange-discover-local (`ed`) | `discover-local.py` | `discover` | **Bespoke.** Local skill-discovery sidecar: scans workspace skills for vault-connected API capabilities (the Exchange can't see the VM's vault). ReadOnly. |
| exchange-evidence-graph-sync (`eg`) | `evidence-graph-sync.py` | `sync` | **Bespoke.** Mirrors ballot/receipt/evidence edges from the D1 mirror into local `evidence-graph.db`. Graph sync, not `ref-kv-store` (edge semantics, sqlite). |
| exchange-intelligence-per-byte (`ei`) | `intelligence-per-byte.py` | `compute` | **Bespoke.** Benchmark harness for Exchange jobs (intel-per-byte metric per MUSE-EXCHANGE-DESIGN.md). Measurement harness; not `ref-model-call`. |
| exchange-mint-claimant (`ek`) | `mint-claimant.py` | `claim` | **Bespoke.** Worker Protocol v1 claimant for lazy artifact minting (PICKUP → POST /v1/claim, delegation validation, lease checks). Exchange-protocol op, not generic `ref-http-fetch`. |
| exchange-mint-submit (`em`) | `mint-submit.py` | `submit` | **Bespoke.** Batch-submits mint jobs from `~/workspace/mint-queue.jsonl` (priority order, artifact-exists check, POST /v1/job with capability "cognitive"). ExternalEffect. |
| exchange-mint-worker (`ew`) | `mint-worker.py` | `execute` | **Bespoke.** Purely deterministic artifact minting: verifies local artifact exists + sha256 matches, reports completion. No LLM, no judgment. Not `ref-hash-seal` (minting protocol, not a seal primitive). |
| exchange-probe-harness (`ep`) | `probe-harness.py` | `probe` | **Bespoke.** Trigger-equivalence campaigns against the governor gates (AUTHORITY/CAPABILITY/EVIDENCE/INTEGRITY/QUOTA/…). Adversarial gate testing. |
| exchange-refresh-mirror (`er`) | `refresh-mirror.py` | `refresh` | **Bespoke.** D1 mirror re-snapshot to `d1-mirror.db` (post-freeze). Write. |

## Reference-organelle check (all 12 considered, none adopted)

- `ref-http-fetch`: mint-claimant/submit POST to Exchange endpoints, but
  their contracts are protocol operations (delegation fields, leases,
  capabilities) — not "fetch a URL".
- `ref-hash-seal`: mint-worker verifies artifact sha256 as one step of a
  minting protocol, not as a seal service.
- `ref-kv-store` / `ref-ledger-append`: evidence-graph-sync mirrors *edges*
  into sqlite; the D1 mirror is a snapshot, not an append log.
- Others: no overlap. Verdict: all ten genuinely bespoke.

## Deliberate exclusions

- `fake-claimant.py`: fake claimant for testing (dev harness).
- `tests/`, `migrations/`: test + migration scripts.
- `d1-mirror.db`, `evidence-graph.db`: data files.
- `audit/`, `campaigns/`, `dogfood-swarm/`: evidence/data dirs.
- `envelope.js`: JS envelope helper (single-file glue; not independently
  deployed — noted, not a unit).

## Execution facts

- Valence: Candidate (network-touching Exchange protocol ops, D1/Cloudflare
  egress, unverified in this harness). `rollup` is ReadOnly.
- network: egress-allowlist (Exchange endpoints, Cloudflare API, D1).
- kill: mint/probe runs are bounded; suspend drains, kill = SIGTERM, final
  receipt from the adapter.
