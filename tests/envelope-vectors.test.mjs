// Cross-language envelope verification tests (SPEC-GAP-PL-007, wave-32).
//
// Proves the JS port (../envelope.js) is byte-faithful to the Rust reference
// and the Python port: every vector in
// ~/workspace/proof-ledger/fixtures/envelope-vectors.json was sealed by
// compute/envelope.py with the PUBLIC test key 0x5a*32 (never the authority
// key). The JS verifier must reproduce the reference verdicts exactly.
//
// Run: node tests/envelope-vectors.test.mjs  (exit 0 = all green)
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  verifyEnvelope, verifyEnvelopeChain, canonicalJSON, EnvelopeError,
  ENV_VERSION,
} from "../envelope.js";

const here = dirname(fileURLToPath(import.meta.url));
const fixtures = JSON.parse(readFileSync(
  join(here, "..", "..", "proof-ledger", "fixtures", "envelope-vectors.json"), "utf8"));
const TEST_KEY = "5a".repeat(32); // PUBLIC test key — hermetic only
const WRONG_KEY = "a5".repeat(32);

let passed = 0;
const ok = (name) => { passed++; console.log("  ok -", name); };

// Vectors that pin known cross-implementation divergences (documented in
// RECEIPT-ENVELOPE.md). They are asserted to FAIL CLOSED, never to verify.
const PINNED = { bigint: "XIMPL-2: integer 2**70 beyond i64 interop range — the Rust reference rejects it too" };

// --- 1. canonical-JSON byte identity against the Python reference ---------
// The fixture's canonical_json is the canonical form of the COMPLETE envelope
// (seal included — it is the auditable wire form, not the seal preimage).
for (const v of fixtures.vectors) {
  if (PINNED[v.id]) { ok(`${v.id}: skipped byte-identity (pinned: ${PINNED[v.id]})`); continue; }
  assert.equal(canonicalJSON(v.envelope), v.canonical_json,
    `${v.id}: canonical JSON differs from the Python reference`);
  ok(`${v.id}: canonical JSON byte-identical to Python reference`);
  // Seal construction: seal === sha256(canonical(envelope minus seal)).
  const noseal = {};
  for (const k of Object.keys(v.envelope)) if (k !== "seal") noseal[k] = v.envelope[k];
  const seal = createHash("sha256").update(canonicalJSON(noseal), "utf8").digest("hex");
  assert.equal(seal, v.envelope.seal, `${v.id}: seal reconstruction mismatch`);
  ok(`${v.id}: seal reconstructs from canonical form`);
}

// --- 2. verify() verdicts match the reference on all 14 vectors -----------
for (const v of fixtures.vectors) {
  if (PINNED[v.id]) {
    await assert.rejects(() => verifyEnvelope(v.envelope, TEST_KEY), EnvelopeError);
    ok(`${v.id}: pinned divergence (${PINNED[v.id]}) — rejected, fail-closed`);
    continue;
  }
  await verifyEnvelope(v.envelope, TEST_KEY);
  ok(`${v.id}: verifies under the test key`);
}

// --- 3. chain vectors link --------------------------------------------------
const chain = ["chain-1", "chain-2", "chain-3"].map(
  (id) => fixtures.vectors.find((v) => v.id === id).envelope);
await verifyEnvelopeChain(chain, TEST_KEY);
ok("chain-1..3: verifyEnvelopeChain green");
await assert.rejects(
  () => verifyEnvelopeChain([chain[0], chain[2]], TEST_KEY), /chain break/);
ok("chain: spliced member detected (chain break)");

// --- 4. negative controls: every forgery class rejected --------------------
const good = fixtures.vectors.find((v) => v.id === "basic-claimed").envelope;
const clone = () => JSON.parse(JSON.stringify(good));

await assert.rejects(() => verifyEnvelope(clone(), WRONG_KEY), /sig mismatch/);
ok("wrong-key envelope rejected");
{
  // Tampering is caught at the sig check (authenticity first — correct).
  // To exercise the payload_hash branch specifically, re-sign the tampered
  // envelope with the test key so the sig is valid but the binding is not.
  const e = clone(); e.payload = { kind: "assertion", forged: true };
  const nosig = {};
  for (const k of Object.keys(e)) if (k !== "seal" && k !== "sig") nosig[k] = e[k];
  const key = await crypto.subtle.importKey("raw",
    Uint8Array.from(Buffer.from(TEST_KEY, "hex")),
    { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
  const raw = await crypto.subtle.sign("HMAC", key,
    new TextEncoder().encode(canonicalJSON(nosig)));
  e.sig = "HS256:" + Buffer.from(raw).toString("hex");
  await assert.rejects(() => verifyEnvelope(e, TEST_KEY), /payload_hash mismatch/);
  ok("tampered payload rejected (payload_hash binding)");
}
{
  const e = clone(); e.seal = "0".repeat(64);
  await assert.rejects(() => verifyEnvelope(e, TEST_KEY), /seal mismatch/);
  ok("tampered seal rejected");
}
{
  const e = clone(); delete e.sig;
  await assert.rejects(() => verifyEnvelope(e, TEST_KEY), /missing sig/);
  ok("missing sig rejected");
}
{
  const e = clone(); e.sig = "ED25519:" + "ab".repeat(32);
  await assert.rejects(() => verifyEnvelope(e, TEST_KEY), /unsupported sig alg/);
  ok("unknown algorithm prefix rejected");
}
{
  const e = clone(); e.sig = "HS256:nothex!";
  await assert.rejects(() => verifyEnvelope(e, TEST_KEY), /sig is not hex/);
  ok("malformed sig rejected");
}
{
  const e = clone(); e.env = "lucidota-receipt/2";
  await assert.rejects(() => verifyEnvelope(e, TEST_KEY), /unsupported env version/);
  ok("wrong env version rejected");
}
{
  const e = clone(); e.status = "verified";
  await assert.rejects(() => verifyEnvelope(e, TEST_KEY), /bad status/);
  ok("non-ternary status rejected");
}
assert.throws(() => canonicalJSON({ x: 1.5 }), /floats are not permitted/);
ok("float in canonical form rejected");
assert.throws(() => canonicalJSON({ n: 2 ** 70 }), /i64 interop range/);
ok("out-of-i64-range integer rejected at canonicalization");
await assert.rejects(() => verifyEnvelope({ not: "an envelope" }, TEST_KEY), /unsupported env version/);
ok("non-envelope object rejected");
await assert.rejects(() => verifyEnvelope(good, "short"), /authority key unavailable/);
ok("malformed authority key fails closed");

console.log(`\n${passed} checks green — JS verify is byte-faithful to the reference`);
