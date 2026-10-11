# exchange-organelles

Tier-B organelle surface for the Exchange — Rust port of
`~/workspace/exchange/organelles/` (adapter.py + the 10 scripts).
One binary, `exchange-organelles`, with `--serve` (unix-socket JSONL
server), `--once` (one JSON request on stdin), and one subcommand per op.
Ops run **natively in-process** (no subprocess); the wire protocol matches
adapter.py exactly, including the interim receipt formula
`adapter-interim:sha256:<hex(canonical output JSON)>`.

## Run

```
# serve the wire protocol (default $XDG_RUNTIME_DIR/organelle-host.sock,
# $ORGANELLE_SOCK or --sock overrides)
exchange-organelles --serve [--sock PATH]

# one request from stdin
echo '{"id":7,"caller":"t","capabilities":[],"codon":"ex","op":"suspend","input":{}}' \
  | exchange-organelles --once

# op subcommands (print output JSON; exit 1 with {"ok":false,"error":{...}} on failure)
exchange-organelles eb-rollup <ballot_id>
exchange-organelles eo-govern
exchange-organelles ed-discover
exchange-organelles eg-sync [--db PATH]
exchange-organelles ei-compute [--latency-rate N]
exchange-organelles ek-claim
exchange-organelles em-submit [--priority N] [--limit N] [--dry-run]
echo '<task json>' | exchange-organelles ew-execute   # exit 2 on malformed task
exchange-organelles ep-probe [--families F1,F4] [--out DIR]
exchange-organelles er-refresh
```

Wire request: `{"id":N,"caller":"...","capabilities":[],"codon":"<2-char>","op":"<op>","input":{...}}`.
Special ops `suspend` (checkpoint note) and `kill` (final receipt, then exit)
are handled by the adapter, as in the Python version.

## Ops

| codon/op | module | port of |
|---|---|---|
| eb/rollup | `ops/eb_rollup.rs` | ballot-cost-rollup.py |
| eo/govern | `ops/eo_govern.rs` | deploy-governor.py |
| ed/discover | `ops/ed_discover.rs` | discover-local.py |
| eg/sync | `ops/eg_sync.rs` | evidence-graph-sync.py |
| ei/compute | `ops/ei_compute.rs` | intelligence-per-byte.py |
| ek/claim | `ops/ek_claim.rs` | mint-claimant.py (mint runs in-process via `ew_execute::mint`) |
| em/submit | `ops/em_submit.rs` | mint-submit.py |
| ew/execute | `ops/ew_execute.rs` | mint-worker.py |
| ep/probe | `ops/ep_probe.rs` | probe-harness.py |
| er/refresh | `ops/er_refresh.rs` | refresh-mirror.py |

## Credentials

`eo/govern` and `er/refresh` auth to the Cloudflare API through the local
authd surrogate flow (`src/creds.rs`, a minimal port of
`/opt/hatch/skills/skill-creator/bin/dynamic_credentials.py`): POST
`{"name":"custom.cloudflare"}` to `/v1/credentials/surrogate` on
`$JARVIS_AUTHD_SOCK`, attach the returned `hsurr:*` surrogate per its
`placement` (bearer header), host-allowlisted to `api.cloudflare.com`.
Surrogate values are never printed or logged. Verified live
(`cargo test -- --ignored creds_live`).

`eb/rollup`, `ed/discover`, `ei/compute`, `ep/probe` use the Exchange bearer
key at `~/.exchange_key`; `ek/claim` uses `~/.exchange_worker_swarm_01`
(two lines: id, secret) — same files as the Python scripts.

## Tests

`cargo test` — 46 tests: wire-protocol units (malformed/unknown/suspend/kill,
receipt determinism), Python-parity integration tests (receipt ids recomputed
by adapter.py on a unicode/float battery; full `--once` response comparison),
ported `test_organelles.py` assertions (manifests, dispatch coverage,
execution.json, `--once` smoke, no-secret scan), per-op units (mint
verdicts, graph sync on fixtures, rollup math, score formula, gate
last-wins, queue ordering), and live socket serve tests.

## Notes

- Canonical JSON for receipts reimplements CPython `json.dumps(sort_keys,
  separators)`: sorted keys, ensure_ascii escaping incl. surrogate pairs,
  and CPython float repr (shortest digits; exponential iff decpt ≤ −4 or
  > 16; signed 2-digit-minimum exponent). Cross-checked against the Python
  adapter on every release-relevant shape.
- Op timeout: 600s wall (worker thread + `recv_timeout`), matching the
  adapter's subprocess timeout; timeout response shape is identical.
- `ei-compute` prints the JSON report (the Python `--json` shape), not the
  human table. `er-refresh` on an empty D1 table skips CREATE instead of
  crashing (Python would raise OperationalError on the COUNT).
