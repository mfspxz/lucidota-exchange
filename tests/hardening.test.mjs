// Hardening acceptance tests (external review 2026-10-10: exchange 7.4/10).
//
// Drives the REAL worker.js against a node:sqlite D1 facade. Each test is a
// HOSTILE test: it fails on the pre-fix code and passes only after the fix.
// Run: node tests/hardening.test.mjs  (exit 0 = all green, exit 1 = any red)
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import assert from "node:assert/strict";
import { createHash, createHmac } from "node:crypto";
import { makeD1 } from "./d1-shim.mjs";
import { canonicalJSON } from "../envelope.js";

const here = dirname(fileURLToPath(import.meta.url));
const TEST_KEY = "5a".repeat(32);
let passed = 0, failed = 0;
const ok = (name) => { passed++; console.log("  ok -", name); };
const bad = (name, note) => { failed++; console.log("  FAIL -", name, note || ""); };

const sha256hex = (s) => createHash("sha256").update(s, "utf8").digest("hex");
const future = () => new Date(Date.now() + 3600e3).toISOString();
let nonceN = 0;
// Mint a distinct test-key-signed envelope per call (nonce => distinct seal).
const mint = (over = {}) => {
  const base = {
    env: "lucidota-receipt/1",
    ts: new Date().toISOString(),
    actor: "luci",
    claim_id: "cA",
    status: "established",
    payload: { kind: "completion", worker_outcome: "SUCCEEDED", nonce: "n" + (++nonceN) },
    prev_hash: "GENESIS",
    ...over,
  };
  const payload_hash = createHash("sha256").update(canonicalJSON(base.payload), "utf8").digest("hex");
  const withHash = { ...base, payload_hash };
  const sig = "HS256:" + createHmac("sha256", Buffer.from(TEST_KEY, "hex"))
    .update(canonicalJSON(withHash), "utf8").digest("hex");
  const noseal = { ...withHash, sig };
  const seal = createHash("sha256").update(canonicalJSON(noseal), "utf8").digest("hex");
  return { ...noseal, seal };
};
const mintObserve = (over = {}) => mint({
  payload: { kind: "observation", resource_id: "r1", ok: true, nonce: "o" + (++nonceN) },
  ...over,
});

// --- seed -------------------------------------------------------------------
const { db } = makeD1();
const seed = [
  ["INSERT INTO providers (id,name,kind,state) VALUES (?,?,?,?)", ["p1", "Test", "test", "UP"]],
  ["INSERT INTO resources (id,provider_id,name,kind,state,cost_class,config) VALUES (?,?,?,?,?,?,?)",
    ["r1", "p1", "TestRes", "test", "AVAILABLE", "free", "{}"]],
  ["INSERT INTO quotas (resource_id,limit_per_min) VALUES (?,?)", ["r1", 60]],
  ["INSERT INTO workers (worker_id,secret_hash) VALUES (?,?)", ["w1", sha256hex("s3cret")]],
  ["INSERT INTO workers (worker_id,secret_hash) VALUES (?,?)", ["w2", sha256hex("s3cret2")]],
  // T1: disproof corroboration — jA/jC/jD running (same input_hash H), jB deferred (same H)
  ["INSERT INTO jobs (id,organelle,capability,spec,state,input_hash) VALUES (?,?,?,?,?,?)",
    ["jA", "test", "inference", JSON.stringify({ capability: "inference", input: { model: "m1" } }), "running", "H"]],
  ["INSERT INTO jobs (id,organelle,capability,spec,state,input_hash) VALUES (?,?,?,?,?,?)",
    ["jB", "test", "inference", JSON.stringify({ capability: "inference", input: { model: "m1" } }), "deferred", "H"]],
  ["INSERT INTO jobs (id,organelle,capability,spec,state,input_hash) VALUES (?,?,?,?,?,?)",
    ["jC", "test", "inference", JSON.stringify({ capability: "inference", input: { model: "m1" } }), "running", "H"]],
  ["INSERT INTO jobs (id,organelle,capability,spec,state,input_hash) VALUES (?,?,?,?,?,?)",
    ["jD", "test", "inference", JSON.stringify({ capability: "inference", input: { model: "m1" } }), "running", "H"]],
  ["INSERT INTO leases (id,job_id,resource_id,state,expires_at) VALUES (?,?,?,?,?)", ["lA", "jA", "r1", "active", future()]],
  ["INSERT INTO leases (id,job_id,resource_id,state,expires_at) VALUES (?,?,?,?,?)", ["lC", "jC", "r1", "active", future()]],
  ["INSERT INTO leases (id,job_id,resource_id,state,expires_at) VALUES (?,?,?,?,?)", ["lD", "jD", "r1", "active", future()]],
  ["INSERT INTO executions (id,job_id,resource_id,claim_id,request_hash) VALUES (?,?,?,?,?)", ["eA", "jA", "r1", "cA", "H"]],
  ["INSERT INTO executions (id,job_id,resource_id,claim_id,request_hash) VALUES (?,?,?,?,?)", ["eC", "jC", "r1", "cC", "H"]],
  ["INSERT INTO executions (id,job_id,resource_id,claim_id,request_hash) VALUES (?,?,?,?,?)", ["eD", "jD", "r1", "cD", "H"]],
  ["INSERT INTO claims (claim_id,job_id,worker_id,lease_id,resource_id,status,lease_expiry,policy_snapshot) VALUES (?,?,?,?,?,?,?,?)",
    ["cA", "jA", "w1", "lA", "r1", "active", future(), "{}"]],
  ["INSERT INTO claims (claim_id,job_id,worker_id,lease_id,resource_id,status,lease_expiry,policy_snapshot) VALUES (?,?,?,?,?,?,?,?)",
    ["cC", "jC", "w2", "lC", "r1", "active", future(), "{}"]],
  ["INSERT INTO claims (claim_id,job_id,worker_id,lease_id,resource_id,status,lease_expiry,policy_snapshot) VALUES (?,?,?,?,?,?,?,?)",
    ["cD", "jD", "w1", "lD", "r1", "active", future(), "{}"]],
  // T2: malformed completion — cX active
  ["INSERT INTO jobs (id,organelle,capability,spec,state) VALUES (?,?,?,?,?)",
    ["jX", "test", "inference", JSON.stringify({ capability: "inference", input: { model: "m1" } }), "running"]],
  ["INSERT INTO leases (id,job_id,resource_id,state,expires_at) VALUES (?,?,?,?,?)", ["lX", "jX", "r1", "active", future()]],
  ["INSERT INTO executions (id,job_id,resource_id,claim_id,request_hash) VALUES (?,?,?,?,?)", ["eX", "jX", "r1", "cX", "HX"]],
  ["INSERT INTO claims (claim_id,job_id,worker_id,lease_id,resource_id,status,lease_expiry,policy_snapshot) VALUES (?,?,?,?,?,?,?,?)",
    ["cX", "jX", "w1", "lX", "r1", "active", future(), "{}"]],
  // T3: crash recovery — cY will be forced into 'completing' with old claimed_at
  ["INSERT INTO jobs (id,organelle,capability,spec,state) VALUES (?,?,?,?,?)",
    ["jY", "test", "inference", JSON.stringify({ capability: "inference", input: { model: "m1" } }), "running"]],
  ["INSERT INTO leases (id,job_id,resource_id,state,expires_at) VALUES (?,?,?,?,?)", ["lY", "jY", "r1", "active", future()]],
  ["INSERT INTO executions (id,job_id,resource_id,claim_id,request_hash) VALUES (?,?,?,?,?)", ["eY", "jY", "r1", "cY", "HY"]],
  ["INSERT INTO claims (claim_id,job_id,worker_id,lease_id,resource_id,status,lease_expiry,policy_snapshot) VALUES (?,?,?,?,?,?,?,?)",
    ["cY", "jY", "w1", "lY", "r1", "active", future(), "{}"]],
];
for (const [sql, params] of seed) await db.prepare(sql).bind(...params).run();

const env = { EXCHANGE_DB: db, EXCHANGE_KEY: "test-admin-key", PL_SIG_KEY: TEST_KEY };
const adminH = { Authorization: "Bearer test-admin-key" };
const w1H = { "x-worker-id": "w1", "x-worker-secret": "s3cret" };
const w2H = { "x-worker-id": "w2", "x-worker-secret": "s3cret2" };

async function postWith(worker, path, body, headers = {}) {
  const req = new Request("https://local" + path, {
    method: "POST",
    headers: { "Content-Type": "application/json", ...headers },
    body: JSON.stringify(body),
  });
  const res = await worker.fetch(req, env);
  let j = null; try { j = await res.json(); } catch { /* non-json */ }
  return { status: res.status, body: j };
}
const jobState = async (id) => (await db.prepare("SELECT state FROM jobs WHERE id=?").bind(id).first()).state;
const claimStatus = async (id) => (await db.prepare("SELECT status FROM claims WHERE claim_id=?").bind(id).first()).status;
const complete = (worker, wbody, wid, wsec, claimId, outcome, output, headers) =>
  postWith(worker, "/v1/complete",
    { worker_id: wid, worker_secret: wsec, claim_id: claimId, worker_outcome: outcome, output,
      envelope: mint({ claim_id: claimId }) }, headers);

// --- T1: disproof corroboration requires DISTINCT eligible workers -------------
{
  const worker = (await import("../worker.js")).default;
  const disproof = { content: "checked: claim is false", ternary_verdict: -1 };

  // 1. w1's -1 on jA: proposal recorded, jB NOT pruned (only 1 witness).
  let r = await complete(worker, null, "w1", "s3cret", "cA", "SUCCEEDED", disproof, w1H);
  if (r.status !== 200) bad("T1a w1 -1 completes", `status=${r.status} ${JSON.stringify(r.body).slice(0,160)}`);
  else {
    ok("T1a w1 -1 completes -> 200");
    if ((await jobState("jB")) === "deferred") ok("T1b single witness: jB stays deferred (no prune)");
    else bad("T1b single witness: jB must stay deferred", await jobState("jB"));
  }

  // 2. HOSTILE: w1's SECOND -1 (different job, same input_hash, same worker)
  //    must NOT prune — one worker's two proposals are not corroboration.
  r = await complete(worker, null, "w1", "s3cret", "cD", "SUCCEEDED", disproof, w1H);
  if (r.status !== 200) bad("T1c w1 second -1 completes", `status=${r.status}`);
  else {
    ok("T1c w1 second -1 completes -> 200");
    if ((await jobState("jB")) === "deferred") ok("T1d hostile: same worker twice -> jB still deferred");
    else bad("T1d hostile: same worker twice must NOT prune", await jobState("jB"));
  }

  // 3. w2's -1: second DISTINCT eligible worker -> prune fires.
  r = await complete(worker, null, "w2", "s3cret2", "cC", "SUCCEEDED", disproof, w2H);
  if (r.status !== 200) bad("T1e w2 -1 completes", `status=${r.status} ${JSON.stringify(r.body).slice(0,160)}`);
  else {
    ok("T1e w2 -1 completes -> 200");
    if ((await jobState("jB")) === "failed") ok("T1f two distinct workers: jB pruned (failed)");
    else bad("T1f two distinct workers must prune jB", await jobState("jB"));
    const pr = await db.prepare(
      "SELECT decision FROM receipts WHERE job_id='jB' AND json_extract(decision,'$.pruned')=1").first();
    if (pr && JSON.parse(pr.decision).pruned === true) ok("T1g prune receipt recorded for jB");
    else bad("T1g prune receipt missing for jB");
    // Prune is idempotent: exactly one prune receipt for jB (no double accounting).
    const n = (await db.prepare(
      "SELECT COUNT(*) c FROM receipts WHERE job_id='jB' AND json_extract(decision,'$.pruned')=1").first()).c;
    if (n === 1) ok("T1h exactly one prune receipt (idempotent)");
    else bad("T1h prune receipt count", `count=${n}`);
  }
}

// --- T2: malformed completion must not strand the claim -------------------------
{
  const worker = (await import("../worker.js")).default;
  const r = await complete(worker, null, "w1", "s3cret", "cX", "BOGUS", { content: "x" }, w1H);
  if (r.status !== 400) bad("T2a invalid outcome -> 400", `status=${r.status}`);
  else ok("T2a invalid worker_outcome -> 400");
  const st = await claimStatus("cX");
  if (st === "active") ok("T2b claim stays 'active' after 400 (not stranded in completing)");
  else bad("T2b claim stranded", `status=${st}`);
  // The claim must still be completable afterwards (no poisoned state).
  const r2 = await complete(worker, null, "w1", "s3cret", "cX", "SUCCEEDED", { content: "done" }, w1H);
  if (r2.status === 200 && (await claimStatus("cX")) === "completed") ok("T2c claim completes normally after the 400");
  else bad("T2c claim must complete after 400", `status=${r2.status} claim=${await claimStatus("cX")}`);
}

// --- T3: crash recovery for stranded 'completing' claims -------------------------
{
  const worker = (await import("../worker.js")).default;
  // Simulate death between the completing-flip and the final UPDATE:
  // claim sits in 'completing' with an old claimed_at.
  await db.prepare(
    "UPDATE claims SET status='completing', claimed_at=datetime('now','-20 minutes') WHERE claim_id='cY'").run();
  if ((await claimStatus("cY")) !== "completing") bad("T3a setup", "claim not forced to completing");
  else {
    ok("T3a simulated crash: claim stuck in 'completing'");
    const r = await postWith(worker, "/v1/admin/sweep-expired-claims", {}, adminH);
    const swept = (r.body && r.body.swept) || [];
    const hit = swept.find((s) => s.claim === "cY" && s.outcome === "RESET_COMPLETING_TO_ACTIVE");
    if (r.status === 200 && hit) ok("T3b sweep recovers stranded claim (RESET_COMPLETING_TO_ACTIVE)");
    else bad("T3b sweep must recover", `status=${r.status} swept=${JSON.stringify(swept).slice(0,200)}`);
    if ((await claimStatus("cY")) === "active") ok("T3c claim back to 'active', worker can retry");
    else bad("T3c claim not reset", await claimStatus("cY"));
  }
}

// --- T4: durable (cross-isolate) replay registry ----------------------------------
{
  // Two isolates = two module records, separate _seenSeals, SHARED D1.
  const isoA = (await import("../worker.js?iso=a")).default;
  const isoB = (await import("../worker.js?iso=b")).default;
  const postA = (body) => postWith(isoA, "/v1/observe", body, adminH);
  const postB = (body) => postWith(isoB, "/v1/observe", body, adminH);

  const e1 = { resource_id: "r1", ok: true, envelope: mintObserve() };
  let r = await postA(e1);
  if (r.status !== 200) bad("T4a first ingest via isoA", `status=${r.status}`);
  else {
    ok("T4a first ingest via isoA -> 200");
    // Same envelope on the OTHER isolate: pre-fix this is 200 (defect);
    // post-fix the D1 registry rejects it.
    r = await postB(e1);
    if (r.status === 409) ok("T4b cross-isolate replay -> 409 (durable registry)");
    else bad("T4b cross-isolate replay must be 409", `status=${r.status}`);
  }

  // Concurrent same-seal race: exactly one wins, the other 409s.
  const e2body = () => ({ resource_id: "r1", ok: true, envelope: mintObserve() });
  const e2 = e2body();
  const [ra, rb] = await Promise.all([postA(e2), postB(e2)]);
  const codes = [ra.status, rb.status].sort().join(",");
  if (codes === "200,409") ok("T4c concurrent same-seal race: exactly one 200, one 409");
  else bad("T4c concurrent race", `statuses=${ra.status},${rb.status}`);

  // Registry is content-addressed, not route-counting: a DISTINCT envelope
  // on the same route still works after replays.
  r = await postA(e2body());
  if (r.status === 200) ok("T4d distinct envelope after replays -> 200");
  else bad("T4d distinct envelope must work", `status=${r.status}`);
}

console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
