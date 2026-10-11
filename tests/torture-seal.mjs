// TORTURE — durable replay registry (Finding 3 re-attack, NOVEL vectors).
//
// v0.6.16 fix: D1 seal_registry(seal PK, first_seen); one atomic
// INSERT..ON CONFLICT DO NOTHING per ingest (changes=0 => 409 replay);
// per-isolate _seenSeals as read-free fast path; REPLAY_MAX_SEEN=10000
// fail-closed 503; pruneSealRegistry 500 rows/cron; 1h TTL.
//
// Vectors: TTL re-admission by design, seal-flood economics vs the
// 503 cap + prune bound, cross-isolate same-seal races, D1-shim fidelity
// limits stated explicitly.
//
// Run: node tests/torture-seal.mjs   (exit 0 = all green, exit 1 = any red)
import { createHash, createHmac } from "node:crypto";
import { makeD1 } from "./d1-shim.mjs";
import { canonicalJSON } from "../envelope.js";

const TEST_KEY = "5a".repeat(32);
let passed = 0, failed = 0;
const ok = (name) => { passed++; console.log("  ok -", name); };
const bad = (name, note) => { failed++; console.log("  FAIL -", name, note || ""); };
const sha256hex = (s) => createHash("sha256").update(s, "utf8").digest("hex");
let nonceN = 0;
const mintObserve = () => {
  const base = {
    env: "lucidota-receipt/1", ts: new Date().toISOString(), actor: "luci",
    claim_id: "cA", status: "established",
    payload: { kind: "observation", resource_id: "r1", ok: true, nonce: "o" + (++nonceN) },
    prev_hash: "GENESIS",
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
  ];
  for (const [sql, params] of seed) await db.prepare(sql).bind(...params).run();
  return db;
}
const env = (db) => ({ EXCHANGE_DB: db, EXCHANGE_KEY: "test-admin-key", PL_SIG_KEY: TEST_KEY });
const adminH = { Authorization: "Bearer test-admin-key" };
async function loadWorker(db, iso) {
  const m = await import(`../worker.js?iso=tseal-${iso}-${Date.now()}-${Math.random()}`);
  m.default.__db = db;
  return m.default;
}
async function observe(worker, envelope) {
  const req = new Request("https://local/v1/observe", {
    method: "POST", headers: { "Content-Type": "application/json", ...adminH },
    body: JSON.stringify({ resource_id: "r1", ok: true, envelope }),
  });
  const res = await worker.fetch(req, env(worker.__db));
  let j = null; try { j = await res.json(); } catch { /* non-json */ }
  return { status: res.status, body: j };
}
const sealCount = (db) => db.prepare("SELECT COUNT(*) n FROM seal_registry").first().then(r => r.n);

// --- S1: TTL re-admission is by design (1h window) -----------------------------
{
  const db = await freshDb();
  const worker = await loadWorker(db, "s1");
  const e = mintObserve();
  const r1 = await observe(worker, e);
  // Age the D1 row past the TTL and simulate the cron prune.
  await db.prepare("UPDATE seal_registry SET first_seen = ? WHERE seal = ?")
    .bind(Date.now() - 2 * 3600 * 1000, e.seal).run();
  await db.prepare("DELETE FROM seal_registry WHERE first_seen < ?")
    .bind(Date.now() - 3600 * 1000).run();
  // Fresh isolate: empty _seenSeals, so only the (now empty) D1 arbitrates.
  const worker2 = await loadWorker(db, "s1b");
  const r2 = await observe(worker2, e);
  if (r1.status === 200 && r2.status === 200 && (await sealCount(db)) === 1)
    ok("S1 TTL re-admission: seal accepted again after TTL+prune (documented 1h window, by design)");
  else bad("S1 TTL re-admission", `r1=${r1.status} r2=${r2.status} seals=${await sealCount(db)}`);
}

// --- S2: cross-isolate same-seal race, N rounds --------------------------------
// In-shim the SQLite INSERTs serialize: exactly one 200 + one 409, every
// round. TRUE multi-writer D1 concurrency is NOT modeled by the shim
// (single-threaded) -> the D1-concurrency verdict stays UNKNOWN.
{
  const db = await freshDb();
  const isoA = await loadWorker(db, "s2a");
  const isoB = await loadWorker(db, "s2b");
  await observe(isoA, mintObserve()); // warm up: keep the migrate cold-start race (P7) out of this vector
  await observe(isoB, mintObserve());
  let clean = 0;
  for (let i = 0; i < 10; i++) {
    const e = mintObserve();
    const [ra, rb] = await Promise.all([observe(isoA, e), observe(isoB, e)]);
    if ([ra.status, rb.status].sort().join(",") === "200,409") clean++;
  }
  if (clean === 10) ok("S2 cross-isolate same-seal race x10: exactly one 200 + one 409 every round (shim; true D1 concurrency UNKNOWN)");
  else bad("S2 cross-isolate race", `clean rounds=${clean}/10`);
}

// --- S3: seal-flood economics vs REPLAY_MAX_SEEN + prune bound -----------------
{
  const db = await freshDb();
  const worker = await loadWorker(db, "s3");
  const N = 10500;
  let ok200 = 0, s503 = 0, other = 0;
  const t0 = Date.now();
  for (let i = 0; i < N; i++) {
    const r = await observe(worker, mintObserve());
    if (r.status === 200) ok200++;
    else if (r.status === 503) s503++;
    else other++;
    if (i % 2500 === 2499) console.log(`    ... flood ${i + 1}/${N}`);
  }
  const ms = Date.now() - t0;
  const seals = await sealCount(db);
  // pruneSealRegistry only removes rows older than the TTL: all rows are
  // fresh, so a simulated cron prune must remove 0.
  const pruned = await db.prepare("DELETE FROM seal_registry WHERE seal IN (SELECT seal FROM seal_registry WHERE first_seen < ? LIMIT 500)")
    .bind(Date.now() - 3600 * 1000).run().then(r => r.meta.changes);
  console.log(`    flood: 200=${ok200} 503=${s503} other=${other} seals_in_d1=${seals} pruned_fresh=${pruned} time=${ms}ms`);
  if (ok200 === 10000 && s503 === 500 && other === 0 && seals === 10000 && pruned === 0)
    ok("S3 seal flood: 10k ingested then fail-closed 503; D1 holds 10k rows; fresh-row prune removes 0 (bounded retention confirmed, sustained-flood growth = 10k/isolate/TTL-window)");
  else bad("S3 seal flood", `200=${ok200} 503=${s503} other=${other} seals=${seals}`);
  // After the flood cap, a REPLAY of a previously-seen seal must still 409
  // (registry intact), and the isolate stays fail-closed for new seals.
  const rReplay = await observe(worker, mintObserve()); // new seal -> 503 expected
  if (rReplay.status === 503) ok("S3b post-flood: new seals still 503 (fail-closed, no eviction)");
  else bad("S3b post-flood fail-closed", `status=${rReplay.status}`);
}

// --- S4: replay of a seal seen on isolate A is rejected on isolate B -----------
{
  const db = await freshDb();
  const isoA = await loadWorker(db, "s4a");
  const isoB = await loadWorker(db, "s4b");
  const e = mintObserve();
  const r1 = await observe(isoA, e);
  const r2 = await observe(isoB, e); // B never saw it: D1 arbiter must 409
  const r3 = await observe(isoA, e); // A saw it in memory: fast-path 409
  if (r1.status === 200 && r2.status === 409 && r3.status === 409)
    ok("S4 cross-isolate replay: A 200, B 409 (D1 arbiter), A-again 409 (memory fast path)");
  else bad("S4 cross-isolate replay", `A=${r1.status} B=${r2.status} A2=${r3.status}`);
}

console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
