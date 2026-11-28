//! v0.7.0 knobs/dials state — port of the worker.js config reads.
//!
//! Operators write settings through POST /v1/config into `key_config` /
//! `exchange_config` in D1. The governor READS them on every govern()
//! call (enabled / allowlist / cost_cap / failover_order / allow_paid);
//! the /v1/capabilities route reads them to compute `key_enabled`.
//!
//! JS semantics preserved: a read against a pre-migration database (no
//! key_config table yet) yields NO knob enforcement — empty map, and
//! allow_paid fail-closed false — never an error that blocks routing.

use crate::db::{Db, Row, RowExt, Val};
use serde_json::Value;
use std::collections::HashMap;

/// One `key_config` row, with the allowlist JSON parsed (JS: unparseable
/// allowlist → null, i.e. no allowlist restriction).
#[derive(Clone, Debug)]
pub struct KeyConfig {
    pub enabled: i64,
    pub allowlist: Option<Vec<String>>,
    pub cost_cap: Option<f64>,
    pub failover_order: i64,
}

impl KeyConfig {
    fn from_row(row: &Row) -> Self {
        let allowlist = match row.get("allowlist") {
            Some(Val::Text(s)) => serde_json::from_str::<Value>(s)
                .ok()
                .and_then(|v| v.as_array().map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect::<Vec<String>>()
                })),
            _ => None,
        };
        KeyConfig {
            enabled: row.i64_or("enabled", 1),
            allowlist,
            cost_cap: row.f64("cost_cap"),
            failover_order: row.i64_or("failover_order", 100),
        }
    }
}

/// The resource config's `key_id` (JS: `cfg.key_id || null`).
pub fn key_id_of(cfg: &Value) -> Option<String> {
    cfg.get("key_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// `SELECT * FROM key_config` → key_id → KeyConfig. On any DB error
/// (pre-migration) returns an empty map, exactly like the JS try/catch.
pub async fn load_key_configs(db: &dyn Db) -> HashMap<String, KeyConfig> {
    let mut out = HashMap::new();
    if let Ok(rows) = db.query_all("SELECT * FROM key_config", &[]).await {
        for row in rows {
            let kid = row.text_or("key_id", "");
            if !kid.is_empty() {
                out.insert(kid, KeyConfig::from_row(&row));
            }
        }
    }
    out
}

/// The global paid-execution kill-switch: true only when the
/// exchange_config row `allow_paid` is exactly "1". Missing table/row →
/// false (fail-closed), like the JS try/catch.
pub async fn load_allow_paid(db: &dyn Db) -> bool {
    match db
        .query_one("SELECT value FROM exchange_config WHERE key='allow_paid'", &[])
        .await
    {
        Ok(Some(row)) => row.text_or("value", "") == "1",
        _ => false,
    }
}
