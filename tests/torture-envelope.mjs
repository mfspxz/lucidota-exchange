// TORTURE — envelope ingest fuzzing (malformed JSON, oversized payloads,
// type confusions, encoding tricks, truncated bodies).
//
// requireEnvelope mapping (worker.js): missing 400 / invalid sig 401 /
// replay 409 / key unprovisioned 503 / registry-full 503 /
// registry-unavailable 503. verifyEnvelope (envelope.js): authenticity
// (HMAC-SHA256 "HS256:<hex>" over canonical JSON minus seal/sig) FIRST,
// then payload binding, then seal.
//
// Run: node tests/torture-envelope.mjs   (exit 0 = all green, exit 1 = any red)
import { createHash, createHmac } from "node:crypto";
import { makeD1 } from "./d1-shim.mjs";
import { canonicalJSON } from "../envelope.js";

const TEST_KEY = "5a".repeat(32);
let passed = 0, failed = 0;
const ok = (name) => { passed++; console.log("  ok -", name); };
const bad = (name, note) => { failed++; console.log("  FAIL -", name, note || ""); };
const sha256hex = (s) => createHash("sha256").update(s, "utf8").digest("hex");
let nonceN = 0;
function mintPayload(payload) {
  const base = {
    env: "lucidota-receipt/1", ts: new Date().toISOString(), actor: "luci",
    claim_id: "cA", status: "established", payload,
    prev_hash: "GENESIS",
  };
  const payload_hash = createHash("sha256").update(canonicalJSON(base.payload), "utf8").digest("hex");
  const withHash = { ...base, payload_hash };
  const sig = "HS256:" + createHmac("sha256", Buffer.from(TEST_KEY, "hex"))
    .update(canonicalJSON(withHash), "utf8").digest("hex");
  const noseal = { ...withHash, sig };
  const seal = createHash("sha256").update(canonicalJSON(noseal), "utf8").digest("hex");
  return { ...noseal, seal };
}
const mintObserve = () => mintPayload({ kind: "observation", resource_id: "r1", ok: true, nonce: "o" + (++nonceN) });
async function freshDb() {
  const { db } = makeD1();
  const seed = [
    ["INSERT INTO providers (id,name,kind,state) VALUES (?,?,?,?)", ["p1", "Test", "test", "UP"]],
    ["INSERT INTO resources (id,provider_id,name,kind,state,cost_class,config) VALUES (?,?,?,?,?,?,?)",
      ["r1", "p1", "TestRes", "test", "AVAILABLE", "free", "{}"]],
    ["INSERT INTO quotas (resource_id,limit_per_min) VALUES (?,?)", ["r1", 60]],
    ["INSERT INTO workers (worker_id,secret_hash) VALUES (?,?)", ["w1", sha256hex("s3cret")]],
  ];
  for (const [sql, params] of seed) await db.prepare(sql).bind(...params).run();
  return db;
}
const env = (db) => ({ EXCHANGE_DB: db, EXCHANGE_KEY: "test-admin-key", PL_SIG_KEY: TEST_KEY });
const adminH = { Authorization: "Bearer test-admin-key" };
async function loadWorker(db, iso) {
  const m = await import(`../worker.js?iso=tenv-${iso}-${Date.now()}-${Math.random()}`);
  m.default.__db = db;
  return m.default;
}
// Raw body post: bypasses JSON.stringify to send malformed/truncated bodies.
async function rawObserve(worker, rawBody, contentType = "application/json") {
  const req = new Request("https://local/v1/observe", {
    method: "POST", headers: { "Content-Type": contentType, ...adminH }, body: rawBody,
  });
  const res = await worker.fetch(req, env(worker.__db));
  let j = null; try { j = await res.json(); } catch { /* non-json */ }
  return { status: res.status, body: j };
}
const E = (envelope) => JSON.stringify({ resource_id: "r1", ok: true, envelope });

const db = await freshDb();
const worker = await loadWorker(db, "fuzz");
await rawObserve(worker, E(mintObserve())); // warmup (migrate race out of scope here)

// --- F1: malformed / truncated bodies ------------------------------------------
{
  const cases = [
    ["empty", "", 400], ["truncated", '{"resource_id":"r1","ok":', 400],
    ["null-body", "null", 400], ["array-body", "[1,2,3]", 400],
    ["string-body", '"hello"', 400], ["number-body", "123", 400],
    ["garbage", "\x00\x01\x02{{{{", 400],
    ["duplicate-keys", '{"resource_id":"r1","resource_id":"r2","ok":true,"envelope":null}', 400],
  ];
  let allOk = true;
  for (const [label, body, want] of cases) {
    const r = await rawObserve(worker, body);
    if (r.status !== want) { allOk = false; bad("F1 malformed body", `${label}: status=${r.status} want=${want}`); }
  }
  if (allOk) ok("F1 malformed/truncated/non-object bodies -> 400 (no 500, no hang)");
}

// --- F2: envelope shape type-confusion ------------------------------------------
{
  const cases = [
    ["missing", undefined], ["null", null], ["string", "sealed"], ["array", []],
    ["number", 42],
  ];
  let allOk = true;
  for (const [label, envelope] of cases) {
    const r = await rawObserve(worker, E(envelope));
    if (r.status !== 400) { allOk = false; bad("F2 envelope shape", `${label}: status=${r.status} want=400`); }
  }
  if (allOk) ok("F2 envelope missing/null/string/array/number -> 400 missing-envelope");
  // {} is present-but-invalid, not missing: the documented mapping says
  // invalid -> 401. Expect 401 here.
  const rEmpty = await rawObserve(worker, E({}));
  if (rEmpty.status === 401) ok("F2b empty-object envelope -> 401 invalid (present but invalid, not missing)");
  else bad("F2b empty-object envelope", `status=${rEmpty.status} want=401`);
}

// --- F3: signature / version / binding attacks -----------------------------------
{
  const good = mintObserve();
  const tamperedPayload = { ...good, payload: { ...good.payload, ok: false, pwned: true } };
  const tamperedSeal = { ...good, seal: "0".repeat(64) };
  const badSig = { ...good, sig: "HS256:" + "1".repeat(64) };
  const noSig = { ...good }; delete noSig.sig;
  const numSig = { ...good, sig: 12345 };
  const badAlg = { ...good, sig: "NONE:deadbeef" };
  const badEnv = { ...good, env: "lucidota-receipt/2" };
  const badStatus = { ...good, status: "super-established" };
  const numSeal = { ...good, seal: 123 };
  const cases = [
    ["tampered-payload", tamperedPayload, 401], ["tampered-seal", tamperedSeal, 401],
    ["bad-sig", badSig, 401], ["missing-sig", noSig, 401], ["numeric-sig", numSig, 401],
    ["bad-alg", badAlg, 401], ["bad-env", badEnv, 401], ["bad-status", badStatus, 401],
    ["numeric-seal", numSeal, 401],
  ];
  let allOk = true;
  for (const [label, envelope, want] of cases) {
    const r = await rawObserve(worker, E(envelope));
    if (r.status !== want) { allOk = false; bad("F3 envelope attacks", `${label}: status=${r.status} want=${want}`); }
  }
  if (allOk) ok("F3 sig/version/binding attacks -> 401 (authenticity first, no bypass)");
}

// --- F4: unicode / encoding ------------------------------------------------------
{
  // Non-BMP characters (emoji) in keys and values: canonicalJSON sorts by
  // UTF-8 byte order (matches the Rust reference); the JS verifier must
  // accept what the JS minter produces.
  const e = mintPayload({ kind: "observation", resource_id: "r1", ok: true, "note-\u{1F600}": "héllo \u{1F600} wörld \u{2028}\u{2029}" });
  const r = await rawObserve(worker, E(e));
  if (r.status === 200) ok("F4 unicode envelope (emoji keys/values, U+2028/29) verifies -> 200");
  else bad("F4 unicode envelope", `status=${r.status} ${JSON.stringify(r.body).slice(0, 160)}`);
  // Lone surrogates are not valid in JSON text; the transport must 400, not 500.
  const r2 = await rawObserve(worker, '{"resource_id":"r1","envelope":"\\ud800"}');
  if (r2.status === 400) ok("F4b lone surrogate body -> 400 (transport rejects, no 500)");
  else bad("F4b lone surrogate", `status=${r2.status}`);
}

// --- F5: oversized payload (no explicit cap — robustness check) -------------------
{
  const big = mintPayload({ kind: "observation", resource_id: "r1", ok: true, blob: "x".repeat(5 * 1024 * 1024) });
  const t0 = Date.now();
  const r = await rawObserve(worker, E(big));
  const ms = Date.now() - t0;
  // Validly signed: must verify (200). The point is no crash/hang/OOM.
  if (r.status === 200) ok(`F5 5MB signed envelope -> 200 in ${ms}ms (no cap, linear verify cost, no crash)`);
  else bad("F5 oversized envelope", `status=${r.status} ${JSON.stringify(r.body).slice(0, 120)}`);
  // Unsigned 5MB garbage: must fail CLOSED (401), not 500, in sane time.
  const garbage = JSON.stringify({ resource_id: "r1", ok: true, envelope: { env: "lucidota-receipt/1", junk: "y".repeat(5 * 1024 * 1024) } });
  const t1 = Date.now();
  const r2 = await rawObserve(worker, garbage);
  const ms2 = Date.now() - t1;
  if ((r2.status === 400 || r2.status === 401) && ms2 < 15000) ok(`F5b 5MB unsigned garbage -> ${r2.status} in ${ms2}ms (fail-closed, linear cost)`);
  else bad("F5b unsigned garbage", `status=${r2.status} time=${ms2}ms`);
}

// --- F6: deeply nested payload -----------------------------------------------------
{
  let deep = { leaf: 1 };
  for (let i = 0; i < 400; i++) deep = { nest: deep };
  let r;
  try {
    const e = mintPayload({ kind: "observation", resource_id: "r1", ok: true, deep });
    r = await rawObserve(worker, E(e));
  } catch (err) { r = { status: "threw", body: { error: String(err).slice(0, 80) } }; }
  if (r.status === 200) ok("F6 400-deep nested payload: verifies -> 200 (no stack blowup)");
  else bad("F6 deep nesting", `status=${r.status} ${JSON.stringify(r.body).slice(0, 120)}`);
}

// --- F7: replay via /v1/observe then /v1/complete sharing one envelope ------------
{
  // An envelope minted for observe must not be reusable as a completion
  // envelope (cross-route replay): the seal registry is route-agnostic.
  const e = mintObserve();
  const r1 = await rawObserve(worker, E(e));
  const req = new Request("https://local/v1/complete", {
    method: "POST", headers: { "Content-Type": "application/json", "x-worker-id": "w1", "x-worker-secret": "s3cret" },
    body: JSON.stringify({ worker_id: "w1", worker_secret: "s3cret", claim_id: "nope",
      worker_outcome: "SUCCEEDED", output: { content: "x" }, envelope: e }),
  });
  const res = await worker.fetch(req, env(worker.__db));
  if (r1.status === 200 && res.status === 409) ok("F7 cross-route replay: observe 200 then same envelope on /v1/complete -> 409");
  else bad("F7 cross-route replay", `observe=${r1.status} complete=${res.status}`);
}

console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
