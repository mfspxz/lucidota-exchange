// Local test harness: runs the REAL ~/workspace/exchange/worker.js (v0.6.6)
// against a node:sqlite-backed D1 facade. No network, no contention.
import { DatabaseSync } from 'node:sqlite';
import http from 'node:http';

const sqlite = new DatabaseSync('/tmp/localgov/exchange-test.db');

const DDL = [
  `CREATE TABLE IF NOT EXISTS providers (id TEXT PRIMARY KEY, name TEXT, kind TEXT, state TEXT, notes TEXT)`,
  `CREATE TABLE IF NOT EXISTS resources (id TEXT PRIMARY KEY, provider_id TEXT, name TEXT, kind TEXT, state TEXT, cost_class TEXT, sovereignty TEXT, config TEXT, evidence_state TEXT, evidence_source TEXT, evidence_at TEXT, cost_basis TEXT, throttled_until TEXT, suspended_until TEXT, credential_state TEXT, claimant_trust_required TEXT)`,
  `CREATE TABLE IF NOT EXISTS resource_capabilities (resource_id TEXT, capability TEXT, detail TEXT)`,
  `CREATE TABLE IF NOT EXISTS quotas (resource_id TEXT PRIMARY KEY, limit_per_min TEXT, measured TEXT, measured_at TEXT, used_window TEXT, window_started_at TEXT, last_checked_at TEXT)`,
  `CREATE TABLE IF NOT EXISTS policies (id TEXT PRIMARY KEY, name TEXT, rule TEXT, effect TEXT)`,
  `CREATE TABLE IF NOT EXISTS jobs (id TEXT PRIMARY KEY, organelle TEXT, capability TEXT, input_hash TEXT, spec TEXT, data_class TEXT, max_cost TEXT, priority TEXT, state TEXT, defer_count TEXT, created_at TEXT DEFAULT (datetime('now')))`,
  `CREATE TABLE IF NOT EXISTS leases (id TEXT PRIMARY KEY, job_id TEXT, resource_id TEXT, state TEXT, authority TEXT, expected_cost TEXT, quota_snapshot TEXT, policy_version TEXT, expires_at TEXT, created_at TEXT DEFAULT (datetime('now')))`,
  `CREATE TABLE IF NOT EXISTS executions (id TEXT PRIMARY KEY, job_id TEXT, resource_id TEXT, started_at TEXT, finished_at TEXT, result TEXT, attempt_no TEXT, request_hash TEXT, lease_id TEXT, claim_id TEXT, failure_taxonomy TEXT, requested_model TEXT, actual_model TEXT, model_match TEXT, usage_json TEXT, output_hash TEXT, error TEXT, error_class TEXT)`,
  `CREATE TABLE IF NOT EXISTS receipts (id TEXT PRIMARY KEY, job_id TEXT, execution_id TEXT, provider TEXT, resource TEXT, started_at TEXT, finished_at TEXT, input_hash TEXT, result TEXT, cost TEXT, policy_version TEXT, decision TEXT, errors TEXT, attempt_id TEXT, output_hash TEXT, cost_before TEXT, cost_after TEXT, quota_before TEXT, quota_after TEXT, health_before TEXT, health_after TEXT, lease TEXT, fallback_chain TEXT, worker_outcome TEXT, claim_id TEXT, ternary_verdict TEXT, ballot_id TEXT, created_at TEXT DEFAULT (datetime('now')))`,
  `CREATE TABLE IF NOT EXISTS health_observations (resource_id TEXT, ok TEXT, latency_ms TEXT, detail TEXT, observed_at TEXT DEFAULT (datetime('now')))`,
];
for (const ddl of DDL) sqlite.exec(ddl);

const NUMRE = /^-?\d+(\.\d+)?$/;
const norm = (row) => {
  if (!row || typeof row !== 'object') return row;
  const o = {};
  for (const [k, v] of Object.entries(row))
    o[k] = (typeof v === 'string' && NUMRE.test(v)) ? Number(v) : v;
  return o;
};

const db = {
  prepare(sql) {
    const stmt = sqlite.prepare(sql);
    const bound = (...params) => ({
      async run() {
        const r = stmt.run(...params);
        return { success: true, meta: { changes: r.changes, last_row_id: Number(r.lastInsertRowid) } };
      },
      async first(col) {
        const row = stmt.get(...params);
        if (row === undefined) return null;
        const n = norm(row);
        return col === undefined ? n : n[col];
      },
      async all() {
        return { results: stmt.all(...params).map(norm) };
      },
    });
    const b0 = bound();
    return { bind: (...p) => bound(...p), run: b0.run, first: b0.first, all: b0.all };
  },
  async batch(list) {
    const out = [];
    for (const s of list) out.push(await s.run());
    return out;
  },
};

const env = { EXCHANGE_DB: db, EXCHANGE_KEY: 'localtest-key' };
const worker = (await import('/home/hatch/workspace/exchange/worker.js')).default;

// ---- seed minimal world ----
const q = (sql, ...p) => sqlite.prepare(sql).run(...p);
const now = new Date().toISOString();
q(`INSERT OR REPLACE INTO providers (id,name,kind,state,notes) VALUES (?,?,?,?,?)`,
  'nvidia', 'NVIDIA', 'inference', 'AVAILABLE', 'local harness');
q(`INSERT OR REPLACE INTO resources (id,provider_id,name,kind,state,cost_class,sovereignty,config,evidence_state,evidence_source,evidence_at,cost_basis,claimant_trust_required) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)`,
  'nim-nvidia', 'nvidia', 'nim', 'inference', 'AVAILABLE', 'free', 'self',
  JSON.stringify({ adapter: 'claimed', model: 'openai/gpt-oss-20b' }),
  'VERIFIED', 'local-harness', now, 'measured', 'any');
q(`INSERT OR REPLACE INTO resource_capabilities (resource_id,capability,detail) VALUES (?,?,?)`,
  'nim-nvidia', 'inference', '{}');

const server = http.createServer(async (req, res) => {
  try {
    const chunks = [];
    for await (const c of req) chunks.push(c);
    const body = Buffer.concat(chunks);
    const headers = {};
    for (const [k, v] of Object.entries(req.headers)) headers[k] = v;
    const request = new Request('http://local' + req.url, {
      method: req.method, headers,
      body: (req.method === 'GET' || req.method === 'HEAD' || !body.length) ? undefined : body,
    });
    const response = await worker.fetch(request, env, { waitUntil() {} });
    const rbody = Buffer.from(await response.arrayBuffer());
    const rh = {};
    response.headers.forEach((v, k) => { rh[k] = v; });
    res.writeHead(response.status, rh);
    res.end(rbody);
  } catch (e) {
    res.writeHead(500, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ harness_error: String(e && e.stack || e).slice(0, 500) }));
  }
});
server.listen(8799, () => console.log('local governor on :8799'));
