//! Database abstraction for exchange-core.
//!
//! Mirrors the Cloudflare D1 surface used by the original worker.js:
//! `prepare(sql).bind(...).first() / .all() / .run()` plus `batch()`.
//! The trait is async so the same core serves both the workers-rs D1
//! binding (async) and the rusqlite test harness (sync-under-async).
//!
//! SQL strings are copied VERBATIM from worker.js — byte-parity on the
//! storage layer is what makes the behavioral parity proofs meaningful.

use async_trait::async_trait;
use std::collections::HashMap;

/// A single bound parameter / column value. Mirrors D1's value model.
#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Val {
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Val::Int(i) => Some(*i),
            Val::Real(f) => Some(*f as i64),
            Val::Text(s) => s.parse().ok(),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Val::Real(f) => Some(*f),
            Val::Int(i) => Some(*i as f64),
            Val::Text(s) => s.parse().ok(),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Val::Text(s) => Some(s),
            _ => None,
        }
    }
    /// JS `||` semantics for text: None or empty string → default.
    pub fn str_or<'a>(&'a self, d: &'a str) -> &'a str {
        match self {
            Val::Text(s) if !s.is_empty() => s,
            _ => d,
        }
    }
    pub fn is_null(&self) -> bool {
        matches!(self, Val::Null)
    }
}

impl From<&str> for Val {
    fn from(s: &str) -> Self {
        Val::Text(s.to_string())
    }
}
impl From<String> for Val {
    fn from(s: String) -> Self {
        Val::Text(s)
    }
}
impl From<i64> for Val {
    fn from(i: i64) -> Self {
        Val::Int(i)
    }
}
impl From<i32> for Val {
    fn from(i: i32) -> Self {
        Val::Int(i as i64)
    }
}
impl From<f64> for Val {
    fn from(f: f64) -> Self {
        Val::Real(f)
    }
}
impl From<bool> for Val {
    fn from(b: bool) -> Self {
        Val::Int(if b { 1 } else { 0 })
    }
}

/// A result row: column name → value. Mirrors D1's row objects.
pub type Row = HashMap<String, Val>;

/// Row accessors with JS-ish semantics.
pub trait RowExt {
    fn get(&self, col: &str) -> Val;
    fn text(&self, col: &str) -> Option<String>;
    fn text_or(&self, col: &str, d: &str) -> String;
    fn i64(&self, col: &str) -> Option<i64>;
    fn i64_or(&self, col: &str, d: i64) -> i64;
    fn f64(&self, col: &str) -> Option<f64>;
}

impl RowExt for Row {
    fn get(&self, col: &str) -> Val {
        self.get(col).cloned().unwrap_or(Val::Null)
    }
    fn text(&self, col: &str) -> Option<String> {
        match self.get(col) {
            Some(Val::Text(s)) if !s.is_empty() => Some(s.clone()),
            _ => None,
        }
    }
    fn text_or(&self, col: &str, d: &str) -> String {
        self.text(col).unwrap_or_else(|| d.to_string())
    }
    fn i64(&self, col: &str) -> Option<i64> {
        self.get(col).and_then(|v| v.as_i64())
    }
    fn i64_or(&self, col: &str, d: i64) -> i64 {
        self.i64(col).unwrap_or(d)
    }
    fn f64(&self, col: &str) -> Option<f64> {
        self.get(col).and_then(|v| v.as_f64())
    }
}

#[derive(Debug, Clone)]
pub struct DbError(pub String);

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "db error: {}", self.0)
    }
}
impl std::error::Error for DbError {}

/// Minimal D1-shaped database interface.
#[async_trait(?Send)]
pub trait Db: Send + Sync {
    /// `prepare(sql).bind(...).first()` — first row or None.
    async fn query_one(&self, sql: &str, params: &[Val]) -> Result<Option<Row>, DbError>;
    /// `prepare(sql).bind(...).all()` — all rows.
    async fn query_all(&self, sql: &str, params: &[Val]) -> Result<Vec<Row>, DbError>;
    /// `prepare(sql).bind(...).run()` — returns rows changed.
    async fn exec(&self, sql: &str, params: &[Val]) -> Result<u64, DbError>;
    /// `db.batch([...])` — atomic multi-statement commit.
    async fn batch(&self, stmts: Vec<(String, Vec<Val>)>) -> Result<(), DbError>;
}

/// Convert a Row to serde_json::Value (object), for response building.
pub fn row_to_json(row: &Row) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for (k, v) in row {
        m.insert(
            k.clone(),
            match v {
                Val::Null => serde_json::Value::Null,
                Val::Int(i) => serde_json::Value::from(*i),
                Val::Real(f) => serde_json::Number::from_f64(*f)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null),
                Val::Text(s) => serde_json::Value::from(s.clone()),
                Val::Blob(b) => serde_json::Value::from(hex::encode(b)),
            },
        );
    }
    serde_json::Value::Object(m)
}
