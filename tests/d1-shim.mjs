// D1 facade over node:sqlite for local worker tests (SPEC-GAP-PL-007, wave-32).
// Pattern proven by redteam2-ballot-forgery/shim.mjs: run the REAL worker.js
// against an in-memory SQLite DB. Base-table DDL mirrors d1-mirror.db (the
// local D1 schema mirror); worker.js migrate() adds/ensures the rest.
import { DatabaseSync } from "node:sqlite";

const DDL = [
  `CREATE TABLE providers (id TEXT PRIMARY KEY, name TEXT, kind TEXT, state TEXT, notes TEXT)`,
  `CREATE TABLE resources (
     id TEXT PRIMARY KEY, provider_id TEXT NOT NULL, name TEXT NOT NULL, kind TEXT NOT NULL,
     state TEXT NOT NULL DEFAULT 'UNKNOWN', cost_class TEXT NOT NULL DEFAULT 'unknown',
     sovereignty TEXT NOT NULL DEFAULT 'external', config TEXT NOT NULL DEFAULT '{}',
     created_at TEXT DEFAULT (datetime('now')))`,
  `CREATE TABLE resource_capabilities (resource_id TEXT, capability TEXT, detail TEXT)`,
  `CREATE TABLE quotas (
     resource_id TEXT PRIMARY KEY, limit_per_min INTEGER, used_window INTEGER DEFAULT 0,
     window_started_at TEXT, quota_remaining_pct REAL, last_checked_at TEXT,
     measured INTEGER DEFAULT 0, measured_at TEXT)`,
  `CREATE TABLE policies (id TEXT PRIMARY KEY, name TEXT, rule TEXT, effect TEXT)`,
  `CREATE TABLE jobs (
     id TEXT PRIMARY KEY, organelle TEXT NOT NULL, capability TEXT NOT NULL,
     input_hash TEXT, spec TEXT NOT NULL, data_class TEXT NOT NULL DEFAULT 'public',
     max_cost REAL NOT NULL DEFAULT 0, priority INTEGER DEFAULT 5,
     state TEXT NOT NULL DEFAULT 'submitted', created_at TEXT DEFAULT (datetime('now')),
     defer_count INTEGER DEFAULT 0)`,
  `CREATE TABLE leases (
     id TEXT PRIMARY KEY, job_id TEXT NOT NULL, resource_id TEXT NOT NULL,
     state TEXT NOT NULL DEFAULT 'active', created_at TEXT DEFAULT (datetime('now')),
     authority TEXT, expected_cost REAL DEFAULT 0, quota_snapshot TEXT,
     policy_version TEXT, expires_at TEXT)`,
  `CREATE TABLE executions (
     id TEXT PRIMARY KEY, job_id TEXT NOT NULL, resource_id TEXT NOT NULL,
     started_at TEXT DEFAULT (datetime('now')), finished_at TEXT, result TEXT,
     output_hash TEXT, error TEXT, error_class TEXT, attempt_no INTEGER,
     lease_id TEXT, failure_taxonomy TEXT, request_hash TEXT, claim_id TEXT,
     requested_model TEXT, actual_model TEXT, model_match INTEGER, usage_json TEXT)`,
  `CREATE TABLE receipts (
     id TEXT PRIMARY KEY, job_id TEXT NOT NULL, execution_id TEXT,
     provider TEXT, resource TEXT, started_at TEXT, finished_at TEXT,
     input_hash TEXT, output_hash TEXT, result TEXT, cost REAL DEFAULT 0,
     quota_before TEXT, quota_after TEXT, health TEXT, decision TEXT,
     errors TEXT, created_at TEXT DEFAULT (datetime('now')),
     attempt_id TEXT, policy_version TEXT, lease TEXT,
     cost_before REAL, cost_after REAL, health_before TEXT, health_after TEXT,
     fallback_chain TEXT, claim_id TEXT, worker_outcome TEXT,
     ternary_verdict INTEGER, ballot_id TEXT)`,
  // Tables owned by worker.js migrate() — duplicated here so the seed can
  // run before the first request triggers migrate(); all IF NOT EXISTS.
  `CREATE TABLE IF NOT EXISTS evidence_log (
     id INTEGER PRIMARY KEY AUTOINCREMENT, at TEXT DEFAULT (datetime('now')),
     resource_id TEXT, from_state TEXT, to_state TEXT, trigger TEXT, detail TEXT)`,
  `CREATE TABLE IF NOT EXISTS workers (
     worker_id TEXT PRIMARY KEY, secret_hash TEXT NOT NULL,
     trust_class TEXT DEFAULT 'remote', name TEXT,
     created_at TEXT DEFAULT (datetime('now')), last_seen_at TEXT,
     disabled INTEGER DEFAULT 0)`,
  `CREATE TABLE IF NOT EXISTS claims (
     claim_id TEXT PRIMARY KEY, job_id TEXT NOT NULL, worker_id TEXT NOT NULL,
     lease_id TEXT NOT NULL UNIQUE, resource_id TEXT NOT NULL,
     attempt_no INTEGER DEFAULT 1, status TEXT DEFAULT 'active',
     claimed_at TEXT DEFAULT (datetime('now')), lease_expiry TEXT NOT NULL,
     policy_snapshot TEXT, receipt_requirements TEXT,
     idempotency_key TEXT, input_hash TEXT,
     worker_outcome TEXT, completed_at TEXT, completion_summary TEXT)`,
  `CREATE TABLE IF NOT EXISTS accounting (
     id TEXT PRIMARY KEY, execution_id TEXT NOT NULL, claim_id TEXT,
     job_id TEXT, resource_id TEXT, measured_cost REAL DEFAULT 0,
     cost_basis TEXT DEFAULT 'unknown', quota_effect TEXT,
     recorded_at TEXT DEFAULT (datetime('now')))`,
  `CREATE TABLE IF NOT EXISTS ballots (
     id TEXT PRIMARY KEY, ballot_type TEXT NOT NULL, proposition TEXT NOT NULL,
     voters TEXT NOT NULL, quorum INTEGER NOT NULL, deadline_at TEXT NOT NULL,
     state TEXT DEFAULT 'open', verdict TEXT, tally TEXT,
     created_at TEXT DEFAULT (datetime('now')))`,
  `CREATE TABLE IF NOT EXISTS ballot_policy (
     id TEXT PRIMARY KEY, voters TEXT NOT NULL, quorum_rule TEXT DEFAULT 'majority',
     deadline_s INTEGER DEFAULT 600, tie_policy TEXT DEFAULT 'unknown')`,
  `CREATE TABLE IF NOT EXISTS health_observations (
     id INTEGER PRIMARY KEY AUTOINCREMENT, resource_id TEXT NOT NULL,
     ok INTEGER NOT NULL, latency_ms INTEGER, detail TEXT,
     observed_at TEXT DEFAULT (datetime('now')))`,
  `CREATE TABLE IF NOT EXISTS envelope_rejections (
     id INTEGER PRIMARY KEY AUTOINCREMENT, at TEXT, route TEXT, reason TEXT,
     claim_id TEXT, actor TEXT)`,
];

export function makeD1() {
  const sqlite = new DatabaseSync(":memory:");
  for (const ddl of DDL) sqlite.exec(ddl);
  const db = {
    prepare(sql) {
      const stmt = sqlite.prepare(sql);
      const bound = (...params) => ({
        async run() {
          const r = stmt.run(...params);
          return { success: true, meta: { changes: Number(r.changes), last_row_id: Number(r.lastInsertRowid) } };
        },
        async first(col) {
          const row = stmt.get(...params);
          if (row === undefined) return null;
          return col === undefined ? row : row[col];
        },
        async all() {
          return { results: stmt.all(...params) };
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
  return { db, sqlite };
}
