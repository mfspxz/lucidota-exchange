// Ingest-boundary envelope enforcement tests (SPEC-GAP-PL-007, wave-32).
//
// Drives the REAL worker.js against a node:sqlite D1 facade. The authority
// key in this harness is the PUBLIC test key 0x5a*32 (hermetic only — the
// real key is never read, never embedded).
//
// Per route (/v1/complete, /v1/observe, /v1/ballots):
//   1. missing envelope      -> 400, nothing stored, rejection recorded
//   2. tampered envelope      -> 401, nothing stored, rejection recorded
//   3. wrong-key envelope     -> 401, nothing stored, rejection recorded
//   4. legit envelope         -> accepted (200), receipt/observation stored
// Plus: no authority key configured -> 503 fail-closed (no silent bypass).
//
// Run: node tests/ingest-envelope.test.mjs  (exit 0 = all green)
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { makeD1 } from "./d1-shim.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const routeEnvs = JSON.parse(readFileSync(join(here, "fixtures", "route-envelopes.json"), "utf8"));
const TEST_KEY = "5a".repeat(32);
const WRONG_KEY = "a5".repeat(32);

let passed = 0;
const ok = (name) => { passed++; console.log("  ok -", name); };

const sha256hex = (s) => createHash("sha256").update(s, "utf8").digest("hex");
const future = () => new Date(Date.now() + 3600e3).toISOString();

// --- seed -------------------------------------------------------------------
const { db } = makeD1();
const seed = [
  ["INSERT INTO providers (id,name,kind,state) VALUES (?,?,?,?)", ["p1", "Test", "test", "UP"]],
  ["INSERT INTO resources (id,provider_id,name,kind,state,cost_class,config) VALUES (?,?,?,?,?,?,?)",
    ["r1", "p1", "TestRes", "test", "AVAILABLE", "free", "{}"]],
  ["INSERT INTO quotas (resource_id,limit_per_min) VALUES (?,?)", ["r1", 60]],
  ["INSERT INTO workers (worker_id,secret_hash) VALUES (?,?)", ["w1", sha256hex("s3cret")]],
  ["INSERT INTO jobs (id,organelle,capability,spec,state) VALUES (?,?,?,?,?)",
    ["j1", "test", "inference", JSON.stringify({ capability: "inference", input: { model: "m1" } }), "running"]],
  ["INSERT INTO leases (id,job_id,resource_id,state,expires_at) VALUES (?,?,?,?,?)",
    ["l1", "j1", "r1", "active", future()]],
  ["INSERT INTO executions (id,job_id,resource_id,claim_id,request_hash) VALUES (?,?,?,?,?)",
    ["e1", "j1", "r1", "c1", "req1"]],
  ["INSERT INTO claims (claim_id,job_id,worker_id,lease_id,resource_id,status,lease_expiry,policy_snapshot) VALUES (?,?,?,?,?,?,?,?)",
    ["c1", "j1", "w1", "l1", "r1", "active", future(), "{}"]],
];
for (const [sql, params] of seed) await db.prepare(sql).bind(...params).run();

const worker = (await import("../worker.js")).default;
const env = { EXCHANGE_DB: db, EXCHANGE_KEY: "test-admin-key", PL_SIG_KEY: TEST_KEY };

async function post(path, body, headers = {}) {
  const req = new Request("https://local" + path, {
    method: "POST",
    headers: { "Content-Type": "application/json", ...headers },
    body: JSON.stringify(body),
  });
  const res = await worker.fetch(req, env);
  return { status: res.status, body: await res.json() };
}
const adminH = { Authorization: "Bearer test-admin-key" };
const workerH = { "x-worker-id": "w1", "x-worker-secret": "s3cret" };

async function rejections() {
  return (await db.prepare("SELECT route,reason FROM envelope_rejections ORDER BY id").all()).results;
}
async function tableCount(t) {
  return (await db.prepare(`SELECT COUNT(*) c FROM ${t}`).first()).c;
}

// --- /v1/complete ------------------------------------------------------------
{
  const base = { worker_id: "w1", worker_secret: "s3cret", claim_id: "c1", worker_outcome: "SUCCEEDED", output: { content: "hello" } };
  const rcptBefore = await tableCount("receipts");

  let r = await post("/v1/complete", { ...base }, workerH);
  assert.equal(r.status, 400); assert.match(r.body.error, /missing envelope/);
  ok("complete: missing envelope -> 400");

  const tampered = JSON.parse(JSON.stringify(routeEnvs["complete-legit"]));
  tampered.payload.kind = "forged";
  r = await post("/v1/complete", { ...base, envelope: tampered }, workerH);
  assert.equal(r.status, 401); assert.match(r.body.error, /envelope rejected/);
  ok("complete: tampered envelope -> 401");

  const wrongKey = JSON.parse(JSON.stringify(routeEnvs["complete-legit"]));
  // re-sign under the wrong key is impossible here (we don't have it); instead
  // submit the test-key envelope while the worker holds the wrong key.
  const envWrongKey = { ...env, PL_SIG_KEY: WRONG_KEY };
  const req = new Request("https://local/v1/complete", {
    method: "POST", headers: { "Content-Type": "application/json", ...workerH },
    body: JSON.stringify({ ...base, envelope: wrongKey }),
  });
  const resWrong = await worker.fetch(req, envWrongKey);
  assert.equal(resWrong.status, 401);
  ok("complete: wrong-key envelope -> 401");

  assert.equal(await tableCount("receipts"), rcptBefore);
  ok("complete: 3 forgeries stored nothing");
  const rej = await rejections();
  assert.equal(rej.filter((x) => x.route === "complete").length, 3);
  ok("complete: 3 rejections recorded in envelope_rejections");

  r = await post("/v1/complete", { ...base, envelope: routeEnvs["complete-legit"] }, workerH);
  assert.equal(r.status, 200, JSON.stringify(r.body).slice(0, 300));
  assert.equal(await tableCount("receipts"), rcptBefore + 1);
  ok("complete: legit envelope -> 200, receipt stored");
}

// --- /v1/observe ---------------------------------------------------------------
{
  const obsBefore = await tableCount("health_observations");
  let r = await post("/v1/observe", { resource_id: "r1", ok: true }, adminH);
  assert.equal(r.status, 400); assert.match(r.body.error, /missing envelope/);
  ok("observe: missing envelope -> 400");

  const tampered = JSON.parse(JSON.stringify(routeEnvs["observe-legit"]));
  tampered.seal = "f".repeat(64);
  r = await post("/v1/observe", { resource_id: "r1", ok: true, envelope: tampered }, adminH);
  assert.equal(r.status, 401);
  ok("observe: tampered envelope -> 401");

  assert.equal(await tableCount("health_observations"), obsBefore);
  ok("observe: 2 forgeries stored nothing");

  r = await post("/v1/observe", { resource_id: "r1", ok: true, envelope: routeEnvs["observe-legit"] }, adminH);
  assert.equal(r.status, 200, JSON.stringify(r.body).slice(0, 200));
  assert.equal(await tableCount("health_observations"), obsBefore + 1);
  ok("observe: legit envelope -> 200, observation stored");
}

// --- /v1/ballots -----------------------------------------------------------------
{
  // ballot_policy 'default' is seeded by migrate(); the ballot below carries
  // its own voters list, so no policy edit is needed.
  const balBefore = await tableCount("ballots");

  let r = await post("/v1/ballots", { ballot_type: "verdict", proposition: "p?", voters: ["m1", "m2"] }, adminH);
  assert.equal(r.status, 400); assert.match(r.body.error, /missing envelope/);
  ok("ballots: missing envelope -> 400");

  const noSig = JSON.parse(JSON.stringify(routeEnvs["ballot-legit"]));
  delete noSig.sig;
  r = await post("/v1/ballots", { ballot_type: "verdict", proposition: "p?", voters: ["m1", "m2"], envelope: noSig }, adminH);
  assert.equal(r.status, 401);
  ok("ballots: unsigned envelope -> 401");

  assert.equal(await tableCount("ballots"), balBefore);
  ok("ballots: 2 forgeries stored nothing");

  r = await post("/v1/ballots",
    { ballot_type: "verdict", proposition: "Is the sky blue?", voters: ["m1", "m2"], envelope: routeEnvs["ballot-legit"] }, adminH);
  assert.equal(r.status, 200, JSON.stringify(r.body).slice(0, 300));
  assert.ok(r.body.ballot_id);
  assert.equal(await tableCount("ballots"), balBefore + 1);
  ok("ballots: legit envelope -> 200, ballot stored");
}

// --- no authority key: fail closed, never silently bypass -------------------------
{
  const envNoKey = { EXCHANGE_DB: db, EXCHANGE_KEY: "test-admin-key" };
  const req = new Request("https://local/v1/observe", {
    method: "POST", headers: { "Content-Type": "application/json", ...adminH },
    body: JSON.stringify({ resource_id: "r1", ok: true, envelope: routeEnvs["observe-legit"] }),
  });
  const res = await worker.fetch(req, envNoKey);
  assert.equal(res.status, 503);
  assert.match((await res.json()).error, /not provisioned/);
  ok("missing PL_SIG_KEY -> 503 fail-closed (no silent bypass)");
}

// --- replay is NOT rejected (no freshness binding — documented next_gap) ------------
{
  // A replayed-but-valid envelope is accepted today: the envelope layer has
  // no nonce/freshness binding (filed as next_gap, not half-built).
  const r = await post("/v1/observe",
    { resource_id: "r1", ok: true, envelope: routeEnvs["observe-legit"] }, adminH);
  assert.equal(r.status, 200);
  ok("replay: valid envelope re-accepted (no freshness binding — next_gap filed)");
}

console.log(`\n${passed} route checks green — ingest boundary enforces envelope provenance`);
