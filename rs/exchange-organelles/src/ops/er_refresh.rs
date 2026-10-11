//! er/refresh — D1 mirror re-snapshot to d1-mirror.db (post-freeze).
//! Port of refresh-mirror.py.
//!
//! Method: paginated SELECT * per table through the Cloudflare D1 HTTP API.
//! Auth via the local authd surrogate flow (crate::creds) — raw keys never
//! touched, printed, or logged. Done = row counts match live AND snapshot
//! timestamp updated.

use rusqlite::{params_from_iter, Connection};
use serde_json::Value;

use super::OpError;
use crate::creds;

pub const API: &str = "https://api.cloudflare.com/client/v4";
pub const ACCOUNT: &str = "243aabef9144fd3b8dd334cb19319e75";
pub const DB_UUID: &str = "0ea89801-9b0a-4efa-9d2f-170d7d285e69";
pub const TABLES: &[&str] = &[
    "jobs", "receipts", "ballots", "claims", "executions", "accounting",
    "resources", "workers", "leases", "quotas",
];
pub const MIRROR: &str = "/home/hatch/workspace/exchange/d1-mirror.db";
pub const STAMP: &str = "/home/hatch/workspace/exchange/d1-mirror.snapshot.json";
/// 2026-10-08 00:00 UTC — refresh refused during the freeze.
pub const FREEZE_END: u64 = 1791417600;

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn d1_query(sql: &str, params: Vec<Value>) -> Result<Vec<Value>, OpError> {
    let client = super::http_client(60);
    let url = format!("{API}/accounts/{ACCOUNT}/d1/database/{DB_UUID}/query");
    let body = serde_json::json!({"sql": sql, "params": params}).to_string().into_bytes();
    let resp = creds::cloudflare_send(
        &client,
        reqwest::Method::POST,
        &url,
        Some(body),
        &[("Content-Type", "application/json")],
    )?;
    let val: Value = resp
        .json()
        .map_err(|e| OpError::failed(format!("D1 decode: {e}")))?;
    if val.get("success").and_then(|s| s.as_bool()) != Some(true) {
        let detail: String = serde_json::to_string(val.get("errors").unwrap_or(&Value::Null))
            .unwrap_or_default()
            .chars()
            .take(300)
            .collect();
        return Err(OpError::failed(format!("D1 query failed: {detail}")));
    }
    Ok(val
        .pointer("/result/0/results")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default())
}

/// Bind one D1 JSON value into sqlite: typed binding (mirrors Python
/// sqlite3 passing raw values), dicts/lists JSON-encoded.
fn to_sql(v: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as Sql;
    match v {
        Value::Null => Sql::Null,
        Value::Bool(b) => Sql::Integer(*b as i64),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Sql::Integer(i)
            } else if let Some(f) = n.as_f64() {
                Sql::Real(f)
            } else {
                Sql::Text(n.to_string())
            }
        }
        Value::String(s) => Sql::Text(s.clone()),
        Value::Array(_) | Value::Object(_) => Sql::Text(v.to_string()),
    }
}

pub fn run(_input: Value) -> Result<Value, OpError> {
    if now_unix() < FREEZE_END {
        return Err(OpError::failed(
            "FROZEN: mirror refresh refused until 2026-10-08 00:00 UTC",
        ));
    }
    let tmp = format!("{MIRROR}.new");
    let _ = std::fs::remove_file(&tmp);
    let g = Connection::open(&tmp).map_err(|e| OpError::failed(format!("open {tmp}: {e}")))?;
    let mut total = 0u64;
    let mut manifest = serde_json::Map::new();

    for t in TABLES {
        let mut offset = 0usize;
        let mut cols: Option<Vec<String>> = None;
        loop {
            let page = d1_query(&format!("SELECT * FROM {t} LIMIT 500 OFFSET {offset}"), vec![])?;
            if cols.is_none() {
                if let Some(first) = page.first() {
                    let c: Vec<String> = first
                        .as_object()
                        .map(|o| o.keys().cloned().collect())
                        .unwrap_or_default();
                    let coldefs = c.iter().map(|x| format!("\"{x}\" TEXT")).collect::<Vec<_>>().join(", ");
                    g.execute(&format!("CREATE TABLE \"{t}\" ({coldefs})"), [])
                        .map_err(|e| OpError::failed(format!("create {t}: {e}")))?;
                    cols = Some(c);
                }
            }
            if page.is_empty() {
                break;
            }
            let c = cols.clone().unwrap_or_default();
            let placeholders = std::iter::repeat("?").take(c.len()).collect::<Vec<_>>().join(",");
            for row in &page {
                let vals: Vec<rusqlite::types::Value> =
                    c.iter().map(|k| to_sql(row.get(k).unwrap_or(&Value::Null))).collect();
                g.execute(
                    &format!("INSERT INTO \"{t}\" VALUES ({placeholders})"),
                    params_from_iter(vals.iter().map(|v| v as &dyn rusqlite::types::ToSql)),
                )
                .map_err(|e| OpError::failed(format!("insert {t}: {e}")))?;
            }
            offset += page.len();
            if page.len() < 500 {
                break;
            }
        }
        let n: i64 = if cols.is_some() {
            g.query_row(&format!("SELECT COUNT(*) FROM \"{t}\""), [], |r| r.get(0))
                .map_err(|e| OpError::failed(format!("count {t}: {e}")))?
        } else {
            0
        };
        let live_n = d1_query(&format!("SELECT COUNT(*) c FROM {t}"), vec![])?
            .first()
            .and_then(|r| r.get("c"))
            .and_then(|c| c.as_i64())
            .unwrap_or(-1);
        let matched = n == live_n;
        manifest.insert(
            t.to_string(),
            serde_json::json!({"mirror": n, "live": live_n, "match": matched}),
        );
        total += n as u64;
    }
    g.close().map_err(|e| OpError::failed(format!("close: {e:?}")))?;
    let all_match = manifest.values().all(|v| v.get("match").and_then(|m| m.as_bool()).unwrap_or(false));
    if !all_match {
        return Err(OpError::failed("MISMATCH: not replacing mirror"));
    }
    std::fs::rename(&tmp, MIRROR).map_err(|e| OpError::failed(format!("replace: {e}")))?;
    let stamp = serde_json::json!({
        "snapshot_utc": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "rows": total,
        "tables": manifest,
    });
    std::fs::write(STAMP, serde_json::to_string_pretty(&stamp).unwrap())
        .map_err(|e| OpError::failed(format!("stamp: {e}")))?;
    Ok(stamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freeze_constant() {
        // 2026-10-08 00:00 UTC
        assert_eq!(FREEZE_END, 1791417600);
        assert!(now_unix() > FREEZE_END); // freeze is in the past
    }

    #[test]
    fn to_sql_typing() {
        use rusqlite::types::Value as Sql;
        assert_eq!(to_sql(&Value::Null), Sql::Null);
        assert_eq!(to_sql(&serde_json::json!(true)), Sql::Integer(1));
        assert_eq!(to_sql(&serde_json::json!(7)), Sql::Integer(7));
        assert!(matches!(to_sql(&serde_json::json!(1.5)), Sql::Real(_)));
        assert_eq!(to_sql(&serde_json::json!("x")), Sql::Text("x".into()));
        assert_eq!(
            to_sql(&serde_json::json!({"a":1})),
            Sql::Text(r#"{"a":1}"#.into())
        );
    }
}
