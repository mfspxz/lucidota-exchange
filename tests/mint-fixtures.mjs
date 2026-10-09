// Test-fixture minter for ingest-boundary envelope tests (wave-36).
//
// Mints the route fixtures in tests/fixtures/route-envelopes.json under the
// PUBLIC test key 0x5a*32 (hermetic only — the real authority key is never
// read, never embedded). This is TEST tooling, not a minting path:
// envelope.js stays VERIFY-ONLY by design; the minter reuses its
// canonicalJSON export so fixtures are byte-identical to what the verifier
// computes, and node:crypto for sha256/HMAC.
//
// Why re-mint (wave-36): the wave-32 fixtures carried claim_ids that did not
// match their request bodies (complete-legit claimed "test-claim-1" while
// the test body used claim_id "c1"). That was the transplant gap. The
// fixtures below bind each envelope to its request's claim context:
//
//   complete: envelope.claim_id === request claim_id
//   observe:  envelope.payload.resource_id === request resource_id
//   ballots:  envelope.payload.proposition === request proposition
//
// Transplant fixtures are validly sealed envelopes for a DIFFERENT claim
// context — the negative controls for tests/ingest-envelope.test.mjs.
//
// Run: node tests/mint-fixtures.mjs  (rewrites tests/fixtures/route-envelopes.json,
// preserving the existing "chain" entry byte-for-byte)
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { createHash, createHmac } from "node:crypto";
import { canonicalJSON } from "../envelope.js";

const here = dirname(fileURLToPath(import.meta.url));
const fixturePath = join(here, "fixtures", "route-envelopes.json");
const TEST_KEY = Buffer.from("5a".repeat(32), "hex"); // PUBLIC test key — hermetic only

const sha256hex = (s) => createHash("sha256").update(s, "utf8").digest("hex");
const hmacHex = (s) => createHmac("sha256", TEST_KEY).update(s, "utf8").digest("hex");

function mint({ claim_id, status, actor, ts, payload, prev_hash = "GENESIS" }) {
  const payload_hash = sha256hex(canonicalJSON(payload));
  const nosig = { env: "lucidota-receipt/1", ts, actor, claim_id, status, payload, payload_hash, prev_hash };
  const sig = "HS256:" + hmacHex(canonicalJSON(nosig));
  const seal = sha256hex(canonicalJSON({ ...nosig, sig }));
  return { ...nosig, sig, seal };
}

const T = "2026-10-09T09:00:00Z";

const fresh = {
  "complete-legit": mint({
    claim_id: "c1", status: "established", actor: "luci", ts: T,
    payload: { kind: "completion", worker_outcome: "SUCCEEDED" },
  }),
  "complete-transplant": mint({
    claim_id: "c2", status: "established", actor: "luci", ts: T,
    payload: { kind: "completion", worker_outcome: "SUCCEEDED" },
  }),
  "observe-legit": mint({
    claim_id: "test-observe-1", status: "claimed", actor: "luci", ts: T,
    payload: { kind: "observation", resource_id: "r1", ok: true },
  }),
  "observe-transplant": mint({
    claim_id: "test-observe-1", status: "claimed", actor: "luci", ts: T,
    payload: { kind: "observation", resource_id: "r2", ok: true },
  }),
  "ballot-legit": mint({
    claim_id: "test-ballot-1", status: "claimed", actor: "luci", ts: T,
    payload: { kind: "ballot", ballot_type: "verdict", proposition: "Is the sky blue?" },
  }),
  "ballot-transplant": mint({
    claim_id: "test-ballot-1", status: "claimed", actor: "luci", ts: T,
    payload: { kind: "ballot", ballot_type: "verdict", proposition: "Is the moon made of cheese?" },
  }),
};

// Preserve the existing "chain" entry untouched (not used by route tests,
// kept for fixture-file stability).
const prev = JSON.parse(readFileSync(fixturePath, "utf8"));
const out = { ...fresh };
for (const k of Object.keys(prev)) if (!(k in out)) out[k] = prev[k];
writeFileSync(fixturePath, JSON.stringify(out, null, 1) + "\n");
console.log("minted", Object.keys(fresh).length, "route fixtures ->", fixturePath);
