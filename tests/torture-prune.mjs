// TORTURE — disproof corroboration quorum games (Finding 1 re-attack, NOVEL vectors).
//
// v0.6.16 fix: corroboration = COUNT(DISTINCT eligible worker_id); disabled
// workers excluded; current worker's unstamped proposal counts iff it has not
// already proposed for this input_hash; prune is one D1 batch with
// conditional UPDATEs + deterministic INSERT..ON CONFLICT DO NOTHING.
//
// Vectors here go beyond hardening.test.mjs T1a-T1h: 3-worker games,
// disabled-worker resurrection, disabled-cannot-propose, concurrent double
// -1 (the documented double-read race), raced-prune idempotency, and
// same-worker x3.
//
// Run: node tests/torture-prune.mjs   (exit 0 = all green, exit 1 = any red)
import { fileURLToPath } from "node:url";
import { dirname } from "node:path";
import { createHash, createHmac } from "node:crypto";
import { makeD1 } from "./d1-shim.mjs";
import { canonicalJSON } from "../envelope.js";

const TEST_KEY = "5a".repeat(32);
let passed = 0, failed = 0;
const ok = (name) => { passed++; console.log("  ok -", name); };
const bad = (name, note) => { failed++; console.log("  FAIL -", name, note || ""); };
const sha256hex = (s) => createHash("sha256").update(s, "utf8").digest("hex");
const future = () => new Date(Date.now() + 3600e3).toISOString();
let nonceN = 0;
const mint = (over = {}) => {
  const base = {
    env: "lucidota-receipt/1", ts: new Date().toISOString(), actor: "luci",
    claim_id: "cA", status: "established",
    payload: { kind: "completion", worker_outcome: "SUCCEEDED", nonce: "n" + (++nonceN) },
    prev_hash: "GENESIS", ...over,
  };
  const payload_hash = createHash("sha256").update(canonicalJSON(base.payload), "utf8").digest("hex");
  const withHash = { ...base, payload_hash };
  const sig = "HS256:" + createHmac("sha256", Buffer.from(TEST_KEY, "hex"))
    .update(canonicalJSON(withHash), "utf8").digest("hex");
  const noseal = { ...withHash, sig };
  const seal = createHash("sha256").update(canonicalJSON(noseal), "utf8").digest("hex");
  return { ...noseal, seal };
};

// Build a fresh DB: 4 running jobs (input_hash H, claims cA1..cA4 on w1..w4)
// + 1 deferred victim jB (input_hash H).
async function freshDb() {
  const { db } = makeD1();
  const seed = [
    ["INSERT INTO providers (id,name,kind,state) VALUES (?,?,?,?)", ["p1", "Test", "test", "UP"]],
    ["INSERT INTO resources (id,provider_id,name,kind,state,cost_class,config) VALUES (?,?,?,?,?,?,?)",
      ["r1", "p1", "TestRes", "test", "AVAILABLE", "free", "{}"]],
    ["INSERT INTO quotas (resource_id,limit_per_min) VALUES (?,?)", ["r1", 60]],
  ];
  for (const [wid, sec] of [["w1", "s3cret"], ["w2", "s3cret2"], ["w3", "s3cret3"], ["w4", "s3cret4"]])
    seed.push(["INSERT INTO workers (worker_id,secret_hash) VALUES (?,?)", [wid, sha256hex(sec)]]);
  for (const j of ["jA1", "jA2", "jA3", "jA4"])
    seed.push(["INSERT INTO jobs (id,organelle,capability,spec,state,input_hash) VALUES (?,?,?,?,?,?)",
      [j, "test", "inference", JSON.stringify({ capability: "inference", input: { model: "m1" } }), "running", "H"]]);
  seed.push(["INSERT INTO jobs (id,organelle,capability,spec,state,input_hash) VALUES (?,?,?,?,?,?)",
    ["jB", "test", "inference", JSON.stringify({ capability: "inference", input: { model: "m1" } }), "deferred", "H"]]);
  const pairs = [["jA1", "w1", "cA1", "lA1", "eA1"], ["jA2", "w2", "cA2", "lA2", "eA2"],
                 ["jA3", "w3", "cA3", "lA3", "eA3"], ["jA4", "w4", "cA4", "lA4", "eA4"]];
  for (const [j, w, c, l, e] of pairs) {
    seed.push(["INSERT INTO leases (id,job_id,resource_id,state,expires_at) VALUES (?,?,?,?,?)", [l, j, "r1", "active", future()]]);
    seed.push(["INSERT INTO executions (id,job_id,resource_id,claim_id,request_hash) VALUES (?,?,?,?,?)", [e, j, "r1", c, "H"]]);
    seed.push(["INSERT INTO claims (claim_id,job_id,worker_id,lease_id,resource_id,status,lease_expiry,policy_snapshot) VALUES (?,?,?,?,?,?,?,?)",
      [c, j, w, l, "r1", "active", future(), "{}"]]);
  }
  for (const [sql, params] of seed) await db.prepare(sql).bind(...params).run();
  return db;
}
const SECRETS = { w1: "s3cret", w2: "s3cret2", w3: "s3cret3", w4: "s3cret4" };
const env = (db) => ({ EXCHANGE_DB: db, EXCHANGE_KEY: "test-admin-key", PL_SIG_KEY: TEST_KEY });
const adminH = { Authorization: "Bearer test-admin-key" };
async function postWith(worker, path, body, headers = {}) {
  const req = new Request("https://local" + path, {
    method: "POST", headers: { "Content-Type": "application/json", ...headers },
    body: JSON.stringify(body),
  });
  const res = await worker.fetch(req, env(worker.__db));
  let j = null; try { j = await res.json(); } catch { /* non-json */ }
  return { status: res.status, body: j };
}
const disproof = { content: "checked: claim is false", ternary_verdict: -1 };
async function neg1(worker, wid, claimId) {
  const H = { "x-worker-id": wid, "x-worker-secret": SECRETS[wid] };
  return postWith(worker, "/v1/complete",
    { worker_id: wid, worker_secret: SECRETS[wid], claim_id: claimId,
      worker_outcome: "SUCCEEDED", output: disproof, envelope: mint({ claim_id: claimId }) }, H);
}
async function loadWorker(db, iso) {
  const m = await import(`../worker.js?iso=tprune-${iso}-${Date.now()}-${Math.random()}`);
  m.default.__db = db;
  return m.default;
}
const jobState = (db, id) => db.prepare("SELECT state FROM jobs WHERE id=?").bind(id).first().then(r => r.state);
const pruneCount = (db) => db.prepare(
  "SELECT COUNT(*) c FROM receipts WHERE job_id='jB' AND json_extract(decision,'$.pruned')=1").first().then(r => r.c);
const proposalCount = (db) => db.prepare(
  "SELECT COUNT(*) c FROM receipts WHERE ternary_verdict=-1 AND json_extract(decision,'$.disproof_proposal')=1").first().then(r => r.c);

// --- P1: sequential 3-worker quorum; third -1 must not double-prune --------
{
  const db = await freshDb();
  const worker = await loadWorker(db, "p1");
  let r = await neg1(worker, "w1", "cA1");
  if (r.status === 200 && (await jobState(db, "jB")) === "deferred") ok("P1a w1 -1: 200, jB stays deferred");
  else bad("P1a w1 -1", `status=${r.status} jB=${await jobState(db, "jB")}`);
  r = await neg1(worker, "w2", "cA2");
  if (r.status === 200 && (await jobState(db, "jB")) === "failed" && (await pruneCount(db)) === 1)
    ok("P1b w2 -1: quorum -> jB pruned, exactly 1 prune receipt");
  else bad("P1b w2 -1 quorum", `status=${r.status} jB=${await jobState(db, "jB")} prunes=${await pruneCount(db)}`);
  r = await neg1(worker, "w3", "cA3");
  if (r.status === 200 && (await pruneCount(db)) === 1) ok("P1c w3 -1 after prune: still exactly 1 prune receipt (no double-prune)");
  else bad("P1c third -1 double-prune", `status=${r.status} prunes=${await pruneCount(db)}`);
}

// --- P2: disabled-worker resurrection game -----------------------------------
{
  const db = await freshDb();
  const worker = await loadWorker(db, "p2");
  let r = await neg1(worker, "w1", "cA1"); // w1 proposes (stamped)
  if (r.status !== 200) bad("P2 setup w1 -1", `status=${r.status}`);
  await db.prepare("UPDATE workers SET disabled=1 WHERE worker_id='w1'").run();
  r = await neg1(worker, "w2", "cA2"); // w1's vote must not count while disabled
  if (r.status === 200 && (await jobState(db, "jB")) === "deferred") ok("P2a w1 disabled: w2 -1 does NOT prune (vote not resurrected)");
  else bad("P2a disabled vote counted", `status=${r.status} jB=${await jobState(db, "jB")}`);
  await db.prepare("UPDATE workers SET disabled=0 WHERE worker_id='w1'").run();
  r = await neg1(worker, "w3", "cA3"); // w1 re-enabled: w1+w2 = 2 distinct -> prune
  if (r.status === 200 && (await jobState(db, "jB")) === "failed" && (await pruneCount(db)) === 1)
    ok("P2b w1 re-enabled: w3 -1 prunes (live eligibility, 2 distinct), 1 receipt");
  else bad("P2b re-enabled quorum", `status=${r.status} jB=${await jobState(db, "jB")} prunes=${await pruneCount(db)}`);
}

// --- P3: a disabled worker cannot propose at all ------------------------------
{
  const db = await freshDb();
  const worker = await loadWorker(db, "p3");
  await db.prepare("UPDATE workers SET disabled=1 WHERE worker_id='w1'").run();
  const r = await neg1(worker, "w1", "cA1");
  if (r.status === 403 && (await jobState(db, "jB")) === "deferred" && (await proposalCount(db)) === 0)
    ok("P3 disabled worker -1 -> 403, no proposal stamped, jB deferred");
  else bad("P3 disabled worker completed", `status=${r.status} proposals=${await proposalCount(db)}`);
}

// --- P4: CONCURRENT double -1 — the documented double-read race ---------------
// NOTE: the isolate is warmed up first so the migrate() cold-start race
// (P7) does not pollute this vector.
{
  const db = await freshDb();
  const worker = await loadWorker(db, "p4");
  await postWith(worker, "/v1/observe",
    { resource_id: "r1", ok: true, envelope: mint({ claim_id: "warm" }) }, adminH);
  const [ra, rb] = await Promise.all([neg1(worker, "w1", "cA1"), neg1(worker, "w2", "cA2")]);
  const st = await jobState(db, "jB");
  const pc = await pruneCount(db);
  const props = await proposalCount(db);
  if (ra.status === 200 && rb.status === 200 && pc <= 1) {
    if (st === "deferred" && props === 2)
      ok(`P4 concurrent double -1: FAIL-SAFE — both proposals stamped, no prune (race confirmed, no quorum yet)`);
    else if (st === "failed" && pc === 1)
      ok(`P4 concurrent double -1: second observer saw quorum — pruned once (interleave resolved to quorum)`);
    else bad("P4 unexpected state", `jB=${st} prunes=${pc} proposals=${props}`);
  } else bad("P4 concurrent statuses", `statuses=${ra.status},${rb.status} prunes=${pc}`);
  // The quorum must still be completable afterwards: a third -1 finishes it
  // (or is a harmless no-op if P4 already pruned).
  const r3 = await neg1(worker, "w3", "cA3");
  const pc2 = await pruneCount(db);
  if (r3.status === 200 && (await jobState(db, "jB")) === "failed" && pc2 === 1)
    ok("P4b third -1 completes/keeps quorum: jB failed, still exactly 1 prune receipt");
  else bad("P4b quorum completion", `status=${r3.status} jB=${await jobState(db, "jB")} prunes=${pc2}`);
}

// --- P5: raced prune batches cannot double-account ----------------------------
{
  const db = await freshDb();
  const worker = await loadWorker(db, "p5");
  await postWith(worker, "/v1/observe",
    { resource_id: "r1", ok: true, envelope: mint({ claim_id: "warm" }) }, adminH);
  let r = await neg1(worker, "w1", "cA1"); // one stamped proposal, no prune
  if (r.status !== 200) bad("P5 setup", `status=${r.status}`);
  // w2 and w3 race: both may observe quorum and both run the prune batch.
  const [r2, r3] = await Promise.all([neg1(worker, "w2", "cA2"), neg1(worker, "w3", "cA3")]);
  const pc = await pruneCount(db);
  if (r2.status === 200 && r3.status === 200 && (await jobState(db, "jB")) === "failed" && pc === 1)
    ok("P5 raced prune batches: jB failed, exactly 1 prune receipt (idempotent under race)");
  else bad("P5 raced prune", `statuses=${r2.status},${r3.status} prunes=${pc}`);
  // A fourth -1 with quorum met re-runs the prune path: still 1 receipt.
  const r4 = await neg1(worker, "w4", "cA4");
  if (r4.status === 200 && (await pruneCount(db)) === 1) ok("P5b repeat prune path: still exactly 1 prune receipt");
  else bad("P5b repeat prune", `status=${r4.status} prunes=${await pruneCount(db)}`);
}

// --- P6: same worker -1 on THREE of its OWN jobs: still one witness -----------
{
  const db = await freshDb();
  // Give w1 two more jobs+claims of its own (cA2/cA3 belong to w2/w3).
  for (const [j, c, l, e] of [["jA1b", "cA1b", "lA1b", "eA1b"], ["jA1c", "cA1c", "lA1c", "eA1c"]]) {
    await db.prepare("INSERT INTO jobs (id,organelle,capability,spec,state,input_hash) VALUES (?,?,?,?,?,?)")
      .bind(j, "test", "inference", JSON.stringify({ capability: "inference", input: { model: "m1" } }), "running", "H").run();
    await db.prepare("INSERT INTO leases (id,job_id,resource_id,state,expires_at) VALUES (?,?,?,?,?)")
      .bind(l, j, "r1", "active", future()).run();
    await db.prepare("INSERT INTO executions (id,job_id,resource_id,claim_id,request_hash) VALUES (?,?,?,?,?)")
      .bind(e, j, "r1", c, "H").run();
    await db.prepare("INSERT INTO claims (claim_id,job_id,worker_id,lease_id,resource_id,status,lease_expiry,policy_snapshot) VALUES (?,?,?,?,?,?,?,?)")
      .bind(c, j, "w1", l, "r1", "active", future(), "{}").run();
  }
  const worker = await loadWorker(db, "p6");
  const rss = [await neg1(worker, "w1", "cA1"), await neg1(worker, "w1", "cA1b"), await neg1(worker, "w1", "cA1c")];
  if (rss.every((x) => x.status === 200) && (await jobState(db, "jB")) === "deferred" && (await pruneCount(db)) === 0)
    ok("P6 same worker x3 -1: 200s, jB stays deferred, 0 prune receipts (one witness, not three)");
  else bad("P6 same worker x3", `statuses=${rss.map((x) => x.status)} jB=${await jobState(db, "jB")}`);
}

// --- P7: migrate() cold-start race — concurrent first requests ----------------
// handle(): if (!__migrated) { await migrate(db); __migrated = true; }
// The flag flips only AFTER migrate() resolves, so N concurrent requests on
// a fresh isolate all enter migrate(); the check-then-act ensure() ALTERs
// collide -> "duplicate column name" -> 500 "exchange fault".
{
  let dup500 = 0, rounds = 0;
  for (let round = 0; round < 5; round++) {
    const db = await freshDb();
    const worker = await loadWorker(db, "p7r" + round);
    const burst = [];
    for (let i = 0; i < 4; i++)
      burst.push(postWith(worker, "/v1/observe",
        { resource_id: "r1", ok: true, envelope: mint({ claim_id: "p7" }) }, adminH));
    const res = await Promise.all(burst);
    rounds++;
    for (const r of res)
      if (r.status === 500 && JSON.stringify(r.body).includes("duplicate column name")) dup500++;
  }
  if (dup500 > 0)
    bad("P7 migrate cold-start race", `BROKEN: ${dup500} x 500 "duplicate column name" across ${rounds} fresh-isolate bursts (transient, self-heals on retry)`);
  else ok("P7 migrate cold-start race: no 500s across fresh-isolate bursts");
}

console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
