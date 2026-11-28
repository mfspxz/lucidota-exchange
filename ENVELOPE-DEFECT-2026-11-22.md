# Exchange Wasm Envelope Defect — Root Cause + Fix (2026-11-22)

## Verdict: FIXED and proven live on staging

The staged Rust worker rejected every valid `lucidota-receipt/1` envelope
with `401 sig mismatch`. Root cause was **not** wasm, not the secret path,
and not the minter: it was a **serde_json feature-unification trap** in
`pl::canon`. The fix is in `pl`'s canonicalizer; staging now passes the
envelope gate at exact parity with production, and still fails closed on
tampered envelopes.

## Root cause

`pl::canon::canonical()` (proof-ledger/rs/src/canon.rs) claimed "object
keys sorted" but never sorted anything. It rebuilt each object by
inserting keys in the source map's **iteration order** and relied on
`serde_json::Map` being BTreeMap-backed. That backing holds only while
serde_json's `preserve_order` feature is OFF in the build graph.

`exchange-core/Cargo.toml` declares:

```toml
serde_json = { version = "1", features = ["preserve_order"] }
```

Cargo feature unification turns `preserve_order` on for the **entire
exchange build graph** (native and wasm alike), flipping
`serde_json::Map` to IndexMap (insertion-order) backing. In that graph,
`pl::envelope::verify_with` canonicalizes an externally minted envelope
in **wire order** (the Python minter's dict order: `env, ts, actor, …`),
not the sorted order the HMAC was computed over (`actor, claim_id, env,
…`). Different canonical bytes → different HMAC → `sig mismatch` for
every externally minted envelope, on every envelope-gated route.

Why nothing caught it:

- The **pl standalone graph** has `preserve_order` nowhere
  (`cargo tree -e features -i serde_json`: 0 hits vs 3 in exchange), so
  the reference verifier `pl envelope op=verify` accepts the same
  envelopes.
- **JS** (`envelope.js`) sorts keys explicitly with a UTF-8 byte
  comparator — immune.
- **Python** (`compute/envelope.py`) canonicalizes with `sort_keys=True`
  — immune.
- The **exchange Rust suite** is self-consistent: it mints with
  `wrap_with` and verifies in the same graph, where mint and verify walk
  the same insertion order, so canonical bytes agree with themselves.
  The defect only appears against envelopes minted *outside* the graph —
  i.e., all real claimant traffic. (Hypotheses (b) secret decoding and
  (d) replay-misreport were eliminated by the staging discrimination
  chain; (a)/(c) were the same defect seen from two sides: the bytes
  verified over were a re-serialization whose key order differed.)

## Bisect (deterministic, local, no D1 traffic)

One fixed envelope (Python-minted, throwaway test key, payload with
nested keys + a non-ASCII string) and one fixed key, verified four ways:

| Verifier | Result |
|---|---|
| Python `envelope.verify` | OK |
| JS `verifyEnvelope` | OK |
| `pl::envelope::verify_with`, pl build graph (no preserve_order) | OK |
| `pl::envelope::verify_with`, exchange build graph (preserve_order) | **FAIL — sig mismatch** |

Same code, same envelope, same key; the only variable is the feature
set. The failing case is preserved as a regression test:
`exchange/rs/exchange-core/tests/envelope_wire_repro.rs` (it parses the
envelope from wire-ordered JSON text and asserts verify succeeds; it
also asserts the fixture really is unsorted, so it cannot silently
degenerate in a sorted-map graph).

## The fix

`proof-ledger/rs/src/canon.rs` — canonicalization now sorts keys
**explicitly** instead of trusting the Map backing store:

- New `sorted_entries()` helper: collects object entries and sorts by
  key (`str` order = UTF-8 byte order — the same order BTreeMap, Python
  `sort_keys`, and the JS UTF-8 comparator produce).
- `canonical()` and `canonical_value()` insert in that sorted order, so
  output bytes are identical under either Map backing.
- The header comment now states the real invariant and names this
  defect, so the assumption cannot creep back.

Canonical bytes in the pl (BTreeMap) graph are byte-for-byte unchanged —
the fix only changes behavior in graphs where the old code was wrong.
`exchange-core`'s `preserve_order` declaration was left in place: other
exchange code may rely on insertion-ordered JSON, and with canon fixed
it is no longer a hazard to envelope verification. (Removing it is a
separate, optional cleanup — not needed for correctness.)

## Validation

- `envelope_wire_repro` (exchange graph): FAIL before fix → **PASS** after.
- `v07_parity`: **21/21 PASS**.
- pl full suite (`cargo test --release` in proof-ledger/rs): **all green**
  (61 lib tests incl. canon + envelope, plus all integration suites, 0
  failures) — no behavior change in the reference graph.
- `cargo check --target wasm32-unknown-unknown -p exchange-worker`:
  **PASS** (2 pre-existing warnings).
- Artifact rebuilt with `worker-build --release`:
  `index_bg.wasm` 1,741,900 B, sha256
  `cd52d7e8f367b853a128c9839844acc6699c8c6bbbb461d8a49d2c11a897038d`;
  `index.js` unchanged, sha256 `0c3c24f0…`.
- Redeployed to **staging only** (`lucidota-exchange-staging`, same D1
  binding, via the estate debug deploy copy).

## Live proof on staging (identical harness to the baseline probes)

| Probe | Before fix | After fix | Production (JS) |
|---|---|---|---|
| E1 no envelope | 400 | **400** missing envelope | 400 |
| E2 valid envelope, bogus claim | 401 sig mismatch | **404 unknown claim** (gate passed) | 404 |
| E3 envelope bound to another claim | 401 sig mismatch | **401 claim binding mismatch** (specific error) | 401 binding mismatch |
| E4 tampered payload, stale sig | — | **401 sig mismatch** (fails closed) | — |

P1 (pinned job submit → blocked, governor), P2 (groq claim → 403), C0
(`/v1/config`: allow_paid=0, 11 keys) all match the pre-fix baseline —
no behavioral drift outside the envelope gate.

### Staging traffic disclosure (shared production D1)

11 requests to staging this session: 4 early probes rejected at worker
auth (no worker headers — auth layer only), the 6 parity probes, and
the E4 tamper probe; plus 1 GET `/v1/health` to production as a closing
check. D1 writes: seal_registry inserts for E2/E3 (verify passed →
replay registration, by design), envelope_rejections rows for E1/E3/E4,
one inert blocked job row from P1. The patched worker's gated
`ensure_migrated()` path means per-request cost was the epoch lookup,
not the old ~3,157-row migrate.

## Production safety

- No deploy to `lucidota-exchange`; root `wrangler.toml` untouched.
- Closing check: production `/v1/health` HTTP 200; `worker.js` sha256
  `76b8292854b2ae84402d128524ea6e6a53314b7bd04453474f80d2c5f7c3dcd3`
  and `wrangler.toml` sha256
  `f8a2f13b2c2ba112ac3bea65469ce1f1ff57a78d41035c96d4988f6603827aa5` —
  identical to the flip-staging step-1 backup.
- No git commits, no wsign (per orders).

## What this unblocks / what remains before a flip

- FLIP-STAGING §"What must happen" item 1 is now satisfied: the wasm
  envelope defect is fixed and E1–E3 return the production-correct
  results (E2 404, E3 binding-mismatch 401), with E4 proving fail-closed.
- Still open from that list (unchanged by this fix): item 2 — re-verify
  at least one resource through the evidence gauntlet and run the full
  W1–W10 battery + a live pinned claim/complete against staging (all
  resources are currently governor-blocked, so claim/complete cannot be
  exercised end-to-end against either worker); item 3 — confirm the
  JS-side A3 patch deployment state so both implementations agree on
  schema epoch `v0.7.3-a3-1` at flip time.
