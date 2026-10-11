//! Test harness: rusqlite-backed `Db` implementation.
//!
//! Mirrors `tests/d1-shim.mjs` (node:sqlite D1 facade) — the same SQL the
//! worker.js torture tests run against, now under the Rust core.

use async_trait::async_trait;
use exchange_core::db::{Db, DbError, Row, Val};
use std::collections::HashMap;
use std::sync::Mutex;

pub struct SqliteDb {
    conn: Mutex<rusqlite::Connection>,
}

impl SqliteDb {
    pub fn open_in_memory() -> Self {
        let conn = rusqlite::Connection::open_in_memory().expect("in-memory sqlite");
        SqliteDb { conn: Mutex::new(conn) }
    }
}

fn to_rusqlite(v: &Val) -> rusqlite::types::Value {
    match v {
        Val::Null => rusqlite::types::Value::Null,
        Val::Int(i) => rusqlite::types::Value::Integer(*i),
        Val::Real(f) => rusqlite::types::Value::Real(*f),
        Val::Text(s) => rusqlite::types::Value::Text(s.clone()),
        Val::Blob(b) => rusqlite::types::Value::Blob(b.clone()),
    }
}

fn row_to_map(row: &rusqlite::Row, col_names: &[String]) -> Result<Row, DbError> {
    let mut m = HashMap::new();
    for (i, name) in col_names.iter().enumerate() {
        let v: rusqlite::types::Value = row.get(i).map_err(|e| DbError(e.to_string()))?;
        m.insert(
            name.to_string(),
            match v {
                rusqlite::types::Value::Null => Val::Null,
                rusqlite::types::Value::Integer(i) => Val::Int(i),
                rusqlite::types::Value::Real(f) => Val::Real(f),
                rusqlite::types::Value::Text(s) => Val::Text(s),
                rusqlite::types::Value::Blob(b) => Val::Blob(b),
            },
        );
    }
    Ok(m)
}

#[async_trait(?Send)]
impl Db for SqliteDb {
    async fn query_one(&self, sql: &str, params: &[Val]) -> Result<Option<Row>, DbError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(sql).map_err(|e| DbError(format!("prepare: {} — {}", e, sql)))?;
        let ps: Vec<rusqlite::types::Value> = params.iter().map(to_rusqlite).collect();
        let col_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let mut rows = stmt
            .query(rusqlite::params_from_iter(ps.iter()))
            .map_err(|e| DbError(format!("query: {}", e)))?;
        match rows.next().map_err(|e| DbError(format!("row: {}", e)))? {
            Some(r) => Ok(Some(row_to_map(r, &col_names)?)),
            None => Ok(None),
        }
    }

    async fn query_all(&self, sql: &str, params: &[Val]) -> Result<Vec<Row>, DbError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(sql).map_err(|e| DbError(format!("prepare: {} — {}", e, sql)))?;
        let ps: Vec<rusqlite::types::Value> = params.iter().map(to_rusqlite).collect();
        let col_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let mut rows = stmt
            .query(rusqlite::params_from_iter(ps.iter()))
            .map_err(|e| DbError(format!("query: {}", e)))?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().map_err(|e| DbError(format!("row: {}", e)))? {
            out.push(row_to_map(r, &col_names)?);
        }
        Ok(out)
    }

    async fn exec(&self, sql: &str, params: &[Val]) -> Result<u64, DbError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(sql).map_err(|e| DbError(format!("prepare: {} — {}", e, sql)))?;
        let ps: Vec<rusqlite::types::Value> = params.iter().map(to_rusqlite).collect();
        let n = stmt
            .execute(rusqlite::params_from_iter(ps.iter()))
            .map_err(|e| DbError(format!("exec: {} — {}", e, sql)))?;
        Ok(n as u64)
    }

    async fn batch(&self, stmts: Vec<(String, Vec<Val>)>) -> Result<(), DbError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().map_err(|e| DbError(format!("begin: {}", e)))?;
        for (sql, params) in stmts {
            let ps: Vec<rusqlite::types::Value> = params.iter().map(to_rusqlite).collect();
            tx.execute(&sql, rusqlite::params_from_iter(ps.iter()))
                .map_err(|e| DbError(format!("batch exec: {} — {}", e, sql)))?;
        }
        tx.commit().map_err(|e| DbError(format!("commit: {}", e)))?;
        Ok(())
    }
}

/// Migrate + seed a fresh test database (mirrors the torture-test setup).
pub async fn fresh_db() -> SqliteDb {
    let db = SqliteDb::open_in_memory();
    // Base tables (created out-of-band in live D1; the JS migrate() is
    // purely additive). Complete column sets — migrate() ensure()s no-op.
    for ddl in [
        "CREATE TABLE IF NOT EXISTS providers (id TEXT PRIMARY KEY, name TEXT, kind TEXT, state TEXT, notes TEXT)",
        "CREATE TABLE IF NOT EXISTS resources (id TEXT PRIMARY KEY, provider_id TEXT, name TEXT, kind TEXT, state TEXT, cost_class TEXT, sovereignty TEXT, config TEXT)",
        "CREATE TABLE IF NOT EXISTS resource_capabilities (resource_id TEXT, capability TEXT, detail TEXT)",
        "CREATE TABLE IF NOT EXISTS quotas (resource_id TEXT PRIMARY KEY, limit_per_min INTEGER, used_window INTEGER DEFAULT 0, window_started_at TEXT, last_checked_at TEXT)",
        "CREATE TABLE IF NOT EXISTS policies (id TEXT PRIMARY KEY, name TEXT, rule TEXT, effect TEXT)",
        "CREATE TABLE IF NOT EXISTS jobs (id TEXT PRIMARY KEY, organelle TEXT, capability TEXT, input_hash TEXT, spec TEXT, data_class TEXT, max_cost REAL, priority INTEGER, state TEXT, defer_count INTEGER DEFAULT 0, created_at TEXT DEFAULT (datetime('now')))",
        "CREATE TABLE IF NOT EXISTS leases (id TEXT PRIMARY KEY, job_id TEXT, resource_id TEXT, state TEXT, created_at TEXT DEFAULT (datetime('now')))",
        "CREATE TABLE IF NOT EXISTS executions (id TEXT PRIMARY KEY, job_id TEXT, resource_id TEXT, started_at TEXT, finished_at TEXT, result TEXT, error TEXT, error_class TEXT, output_hash TEXT)",
        "CREATE TABLE IF NOT EXISTS receipts (id TEXT PRIMARY KEY, job_id TEXT, execution_id TEXT, attempt_id TEXT, provider TEXT, resource TEXT, started_at TEXT, finished_at TEXT, input_hash TEXT, output_hash TEXT, result TEXT, cost REAL, quota_before TEXT, quota_after TEXT, policy_version TEXT, lease TEXT, decision TEXT, fallback_chain TEXT, errors TEXT, created_at TEXT DEFAULT (datetime('now')))",
    ] {
        db.exec(ddl, &[]).await.expect("base ddl");
    }
    exchange_core::migrate::migrate(&db).await.expect("migrate");
    db
}

/// Test Env: no exchange key by default (tests set what they need), mock HTTP.
pub fn test_env() -> exchange_core::env::Env {
    exchange_core::env::Env {
        exchange_key: Some("test-admin-key".to_string()),
        secrets: std::collections::HashMap::new(),
        http: std::sync::Arc::new(exchange_core::env::MockHttp::new()),
    }
}
