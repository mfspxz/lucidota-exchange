// TORTURE — completeClaimed validation ordering + claim-lifecycle races
// (Finding 2 re-attack, NOVEL vectors).
//
// v0.6.16 fix: worker_outcome validated BEFORE the active->completing flip;
// 400 leaves the claim 'active'. Crash backstop: sweepExpiredClaims resets
// 'completing' older than 15min (keyed on claimed_at) to 'active'.
//
// Vectors: outcome type-confusion battery, concurrent double-complete,
// sweep-interleaved double-execution (deterministic via instrumented DB),
// sweep-vs-fresh-completion, crash-recovery-then-retry.
//
// Run: node tests/torture-complete.mjs   (exit 0 = all green, exit 1 = any red)
import { createHash, createHmac } from "node:crypto";
import { makeD1 } from "./d1-shim.mjs";
import { canonicalJSON } from "../envelope.js";

const TEST_KEY = "5a".repeat(32);
let passed = 0, failed = 0;
const ok = (name) => { passed++; console.log("  ok -", name); };
const bad = (name, note) => { failed++; console.log("  FAIL -", name, note || ""); };
const sha256hex = (s) => createHash("sha256").update(s, "utf8").digest("hex");
const future = () => new Date(Date.now() + 3600e3).toISOString();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
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

async function freshDb() {
  const { db } = makeD1();
  const seed = [
    ["INSERT INTO providers (id,name,kind,state) VALUES (?,?,?,?)", ["p1", "Test", "test", "UP"]],
    ["INSERT INTO resources (id,provider_id,name,kind,state,cost_class,config) VALUES (?,?,?,?,?,?,?)",
      ["r1", "p1", "TestRes", "test", "AVAILABLE", "free", "{}"]],
    ["INSERT INTO quotas (resource_id,limit_per_min) VALUES (?,?)", ["r1", 60]],
    ["INSERT INTO workers (worker_id,secret_hash) VALUES (?,?)", ["w1", sha256hex("s3cret")]],
    ["INSERT INTO jobs (id,organelle,capability,spec,state) VALUES (?,?,?,?,?)",
      ["jX", "test", "inference", JSON.stringify({ capability: "inference", input: { model: "m1" } }), "running"]],
    ["INSERT INTO leases (id,job_id,resource_id,state,expires_at) VALUES (?,?,?,?,?)", ["lX", "jX", "r1", "active", future()]],
    ["INSERT INTO executions (id,job_id,resource_id,claim_id,request_hash) VALUES (?,?,?,?,?)", ["eX", "jX", "r1", "cX", "HX"]],
    ["INSERT INTO claims (claim_id,job_id,worker_id,lease_id,resource_id,status,lease_expiry,policy_snapshot) VALUES (?,?,?,?,?,?,?,?)",
      ["cX", "jX", "w1", "lX", "r1", "active", future(), "{}"]],
  ];
  for (const [sql, params] of seed) await db.prepare(sql).bind(...params).run();
  return db;
}
// Instrumented DB: every statement execution sleeps `ms`, widening race
// windows deterministically. Pass-through otherwise.
function slowDb(db, ms) {
  const wrap = (fn) => async (...a) => { await sleep(ms); return fn(...a); };
  return {
    prepare(sql) {
      const s = db.prepare(sql);
      const wb = (...p) => { const b = s.bind(...p); return { run: wrap(b.run), first: wrap(b.first), all: wrap(b.all) }; };
      return { bind: wb, run: wrap(s.run), first: wrap(s.first), all: wrap(s.all) };
    },
    batch: (list) => db.batch(list),
  };
}
const env = (db) => ({ EXCHANGE_DB: db, EXCHANGE_KEY: "test-admin-key", PL_SIG_KEY: TEST_KEY });
const adminH = { Authorization: "Bearer test-admin-key" };
const w1H = { "x-worker-id": "w1", "x-worker-secret": "s3cret" };
async function postWith(worker, path, body, headers = {}) {
  const req = new Request("https://local" + path, {
    method: "POST", headers: { "Content-Type": "application/json", ...headers },
    body: JSON.stringify(body),
  });
  const res = await worker.fetch(req, env(worker.__db));
  let j = null; try { j = await res.json(); } catch { /* non-json */ }
  return { status: res.status, body: j };
}
async function loadWorker(db, iso) {
  const m = await import(`../worker.js?iso=tcomp-${iso}-${Date.now()}-${Math.random()}`);
  m.default.__db = db;
  return m.default;
}
const claimStatus = (db, id) => db.prepare("SELECT status FROM claims WHERE claim_id=?").bind(id).first().then(r => r.status);
const receiptsFor = (db, c) => db.prepare("SELECT COUNT(*) n FROM receipts WHERE claim_id=?").bind(c).first().then(r => r.n);
const acctsFor = (db, c) => db.prepare("SELECT COUNT(*) n FROM accounting WHERE claim_id=?").bind(c).first().then(r => r.n);
const completeAs = (worker, claimId, outcome, extra = {}) =>
  postWith(worker, "/v1/complete",
    { worker_id: "w1", worker_secret: "s3cret", claim_id: claimId, worker_outcome: outcome,
      output: { content: "done" }, envelope: mint({ claim_id: claimId }), ...extra }, w1H);

// --- C1: outcome type-confusion battery ---------------------------------------
{
  const db = await freshDb();
  const worker = await loadWorker(db, "c1");
  const cases = [
    ["null", null], ["missing-key", undefined], ["array", ["SUCCEEDED"]],
    ["object", { x: 1 }], ["number", 123], ["trailing-space", "SUCCEEDED "],
    ["empty", ""], ["lowercase", "succeeded"], ["boolean", true],
  ];
  let allOk = true;
  for (const [label, outcome] of cases) {
    const body = { worker_id: "w1", worker_secret: "s3cret", claim_id: "cX",
      output: { content: "done" }, envelope: mint({ claim_id: "cX" }) };
    if (outcome !== undefined) body.worker_outcome = outcome;
    const r = await postWith(worker, "/v1/complete", body, w1H);
    const st = await claimStatus(db, "cX");
    if (r.status !== 400 || st !== "active") { allOk = false; bad("C1 outcome confusion", `${label}: status=${r.status} claim=${st}`); }
  }
  if (allOk) ok("C1 outcome type-confusion: null/missing/array/object/number/padded/empty/lower/bool -> 400, claim stays active");
  const r = await completeAs(worker, "cX", "SUCCEEDED");
  if (r.status === 200 && (await claimStatus(db, "cX")) === "completed") ok("C1b claim still completable after the 400 battery");
  else bad("C1b claim completable after 400s", `status=${r.status}`);
}

// --- C2: concurrent double-complete on one claim ------------------------------
{
  const db = await freshDb();
  const worker = await loadWorker(db, "c2");
  await postWith(worker, "/v1/observe",
    { resource_id: "r1", ok: true, envelope: mint({ claim_id: "warm" }) }, adminH);
  const [ra, rb] = await Promise.all([completeAs(worker, "cX", "SUCCEEDED"), completeAs(worker, "cX", "SUCCEEDED")]);
  const codes = [ra.status, rb.status].sort().join(",");
  const rc = await receiptsFor(db, "cX"), ac = await acctsFor(db, "cX");
  if ((codes === "200,409" || codes === "200,200") && rc === 1 && ac === 1 && (await claimStatus(db, "cX")) === "completed")
    ok(`C2 concurrent double-complete: statuses ${codes}, exactly 1 receipt + 1 accounting row (no double-execution)`);
  else bad("C2 double-complete", `statuses=${codes} receipts=${rc} accounting=${ac}`);
}

// --- C3: sweep interleaved with an in-flight completion -----------------------
// Claim aged past the 15min stuck threshold with a still-valid lease.
// The sweep resets 'completing'->'active' MID-COMPLETION; the in-flight
// completion keeps writing (receipt/accounting/quota are unconditional
// post-flip) and its final conditional UPDATE no-ops. A retry then
// executes a SECOND time: the idempotency guard never fires.
{
  const db = await freshDb();
  await db.prepare("UPDATE claims SET claimed_at=datetime('now','-20 minutes') WHERE claim_id='cX'").run();
  const wrapped = slowDb(db, 15);
  const worker = await loadWorker(wrapped, "c3");
  await postWith(worker, "/v1/observe",
    { resource_id: "r1", ok: true, envelope: mint({ claim_id: "warm" }) }, adminH);
  const p1 = postWith(worker, "/v1/complete",
    { worker_id: "w1", worker_secret: "s3cret", claim_id: "cX", worker_outcome: "SUCCEEDED",
      output: { content: "done" }, envelope: mint({ claim_id: "cX" }) }, w1H);
  let sawCompleting = false;
  for (let i = 0; i < 200 && !sawCompleting; i++) {
    await sleep(5);
    sawCompleting = (await claimStatus(db, "cX")) === "completing";
  }
  let sweepHit = false;
  if (sawCompleting) {
    const sw = await postWith(worker, "/v1/admin/sweep-expired-claims", {}, adminH);
    sweepHit = ((sw.body && sw.body.swept) || []).some((s) => s.claim === "cX" && s.outcome === "RESET_COMPLETING_TO_ACTIVE");
  }
  const r1 = await p1;
  const midStatus = await claimStatus(db, "cX");
  // Worker retry after the (apparent) timeout: fresh envelope.
  const r2 = await completeAs(worker, "cX", "SUCCEEDED");
  const rc = await receiptsFor(db, "cX"), ac = await acctsFor(db, "cX");
  if (sweepHit && rc === 2 && ac === 2)
    bad("C3 sweep-interleaved double-execution",
      `BROKEN: sweep reset mid-completion; attempt#1 -> ${r1.status} (final write no-op, status was '${midStatus}'); retry -> ${r2.status}; receipts=${rc} accounting=${ac} for ONE claim`);
  else if (sweepHit)
    ok(`C3 sweep fired mid-completion but no double-execution (receipts=${rc} accounting=${ac}) — unexpected, investigate`);
  else bad("C3 setup", `sweep did not catch the in-flight completion (sawCompleting=${sawCompleting})`);
}

// --- C4: sweep must NOT touch a fresh in-flight completion --------------------
{
  const db = await freshDb(); // claimed_at = now (fresh)
  const wrapped = slowDb(db, 15);
  const worker = await loadWorker(wrapped, "c4");
  await postWith(worker, "/v1/observe",
    { resource_id: "r1", ok: true, envelope: mint({ claim_id: "warm" }) }, adminH);
  const p1 = completeAs(worker, "cX", "SUCCEEDED");
  let sawCompleting = false;
  for (let i = 0; i < 200 && !sawCompleting; i++) {
    await sleep(5);
    sawCompleting = (await claimStatus(db, "cX")) === "completing";
  }
  let sweepTouched = false;
  if (sawCompleting) {
    const sw = await postWith(worker, "/v1/admin/sweep-expired-claims", {}, adminH);
    sweepTouched = ((sw.body && sw.body.swept) || []).some((s) => s.claim === "cX");
  }
  const r1 = await p1;
  const rc = await receiptsFor(db, "cX");
  if (sawCompleting && !sweepTouched && r1.status === 200 && rc === 1 && (await claimStatus(db, "cX")) === "completed")
    ok("C4 sweep leaves fresh in-flight completion alone: 200, 1 receipt, completed");
  else bad("C4 fresh completion disturbed", `touched=${sweepTouched} status=${r1.status} receipts=${rc}`);
}

// --- C5: crash-recovery path then original-worker retry -----------------------
{
  const db = await freshDb();
  const worker = await loadWorker(db, "c5");
  // Simulate death between flip and final write.
  await db.prepare("UPDATE claims SET status='completing', claimed_at=datetime('now','-20 minutes') WHERE claim_id='cX'").run();
  const sw = await postWith(worker, "/v1/admin/sweep-expired-claims", {}, adminH);
  const hit = ((sw.body && sw.body.swept) || []).some((s) => s.claim === "cX" && s.outcome === "RESET_COMPLETING_TO_ACTIVE");
  const r1 = await completeAs(worker, "cX", "SUCCEEDED");
  const r2 = await completeAs(worker, "cX", "SUCCEEDED"); // "original" worker wakes up, retries
  const rc = await receiptsFor(db, "cX");
  if (hit && r1.status === 200 && r2.status === 200 && r2.body && r2.body.idempotent === true && rc === 1)
    ok("C5 crash-recovery: sweep resets, complete 200s, late retry idempotent-replays, exactly 1 receipt");
  else bad("C5 crash recovery", `hit=${hit} r1=${r1.status} r2=${r2.status} idem=${r2.body && r2.body.idempotent} receipts=${rc}`);
}

// --- C6: worker disabled mid-claim ------------------------------------------------
{
  const db = await freshDb();
  const worker = await loadWorker(db, "c6");
  await db.prepare("UPDATE workers SET disabled=1 WHERE worker_id='w1'").run();
  const r = await completeAs(worker, "cX", "SUCCEEDED");
  const st = await claimStatus(db, "cX");
  // Recovery path: the lease expires -> zombie sweep expires the claim,
  // the job defers for replan. No permanently stuck work.
  await db.prepare("UPDATE claims SET lease_expiry=datetime('now','-1 minute') WHERE claim_id='cX'").run();
  const sw = await postWith(worker, "/v1/admin/sweep-expired-claims", {}, adminH);
  const expired = ((sw.body && sw.body.swept) || []).some((s) => s.claim === "cX" && s.outcome === "EXPIRED");
  const jst = await db.prepare("SELECT state FROM jobs WHERE id='jX'").bind().first().then(x => x.state);
  if (r.status === 403 && st === "active" && expired && jst === "deferred")
    ok("C6 disabled mid-claim: complete -> 403, claim stays active, lease-expiry sweep recovers (EXPIRED, job deferred for replan)");
  else bad("C6 disabled mid-claim", `complete=${r.status} claim=${st} expired=${expired} job=${jst}`);
}

// --- C7: two workers race /v1/claim for one lease ---------------------------------
{
  const db = await freshDb();
  await db.prepare("INSERT INTO workers (worker_id,secret_hash) VALUES (?,?)")
    .bind("w2", sha256hex("s3cret2")).run();
  await db.prepare("INSERT INTO jobs (id,organelle,capability,spec,state) VALUES (?,?,?,?,?)")
    .bind("jQ", "test", "inference", JSON.stringify({ capability: "inference", input: { model: "m1" } }), "deferred").run();
  await db.prepare("INSERT INTO leases (id,job_id,resource_id,state,expires_at) VALUES (?,?,?,?,?)")
    .bind("lQ", "jQ", "r1", "active", future()).run();
  const worker = await loadWorker(db, "c7");
  const claimAs = (wid, sec) => postWith(worker, "/v1/claim",
    { worker_id: wid, worker_secret: sec, resource_id: "r1" },
    { "x-worker-id": wid, "x-worker-secret": sec });
  const [a, b] = await Promise.all([
    claimAs("w1", "s3cret"), claimAs("w2", "s3cret2"),
  ]);
  const wins = [a, b].filter((x) => x.body && x.body.claimed === true).length;
  const claimRows = await db.prepare("SELECT COUNT(*) n FROM claims WHERE lease_id='lQ'").bind().first().then(x => x.n);
  if (wins === 1 && claimRows === 1) ok("C7 claim race: exactly one winner, one claim row (atomic lease win holds)");
  else bad("C7 claim race", `wins=${wins} claimRows=${claimRows} a=${a.status} b=${b.status}`);
}

// --- C8: identical-body double-complete -> replay 409 ------------------------------
{
  const db = await freshDb();
  const worker = await loadWorker(db, "c8");
  const body = { worker_id: "w1", worker_secret: "s3cret", claim_id: "cX",
    worker_outcome: "SUCCEEDED", output: { content: "done" }, envelope: mint({ claim_id: "cX" }) };
  const r1 = await postWith(worker, "/v1/complete", body, w1H);
  const r2 = await postWith(worker, "/v1/complete", body, w1H); // byte-identical replay
  const rc = await receiptsFor(db, "cX");
  if (r1.status === 200 && r2.status === 409 && rc === 1)
    ok("C8 identical-body replay: first 200, replay 409 (seal registry guards /v1/complete too), 1 receipt");
  else bad("C8 identical replay", `r1=${r1.status} r2=${r2.status} receipts=${rc}`);
}

console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
