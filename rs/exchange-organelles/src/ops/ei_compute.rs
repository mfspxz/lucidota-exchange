//! ei/compute — intelligence-per-byte benchmark harness.
//! Port of intelligence-per-byte.py.
//!
//! score = verified / (bytes + LATENCY_BYTE_RATE * latency_s)
//! verified = 1 iff receipt result == "ok" AND a VERIFY gate passed in the
//! governor decision trace. Read-only: receipts + job specs, no mutations.

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{exchange_key, http_client, OpError};

pub const EX: &str = "https://lucidota-exchange.cutecity.workers.dev";
pub const UA: &str = "lucidota-intel-per-byte/1.0";
pub const LATENCY_BYTE_RATE: f64 = 1000.0;

fn get(client: &reqwest::blocking::Client, key: &str, path: &str) -> Result<Value, OpError> {
    let resp = client
        .get(format!("{EX}{path}"))
        .header("Authorization", format!("Bearer {key}"))
        .header("User-Agent", UA)
        .send()
        .map_err(|e| OpError::failed(format!("GET {path}: {e}")))?;
    resp.json::<Value>()
        .map_err(|e| OpError::failed(format!("GET {path} decode: {e}")))
}

/// One pagination step decision, pure and testable.
/// Returns (fresh_rows, stop) for a fetched page.
pub fn page_step(page: &[Value], seen_ids: &mut std::collections::HashSet<String>) -> (Vec<Value>, bool) {
    let fresh: Vec<Value> = page
        .iter()
        .filter(|r| {
            r.get("id")
                .and_then(|i| i.as_str())
                .map(|id| seen_ids.insert(id.to_string()))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    let stop = fresh.is_empty() || page.len() < 50;
    (fresh, stop)
}

/// Walk all pages. The endpoint caps at 50 per page; without this, older
/// receipts fall out of the window. Mirrors get_all_receipts().
pub fn get_all_receipts(
    client: &reqwest::blocking::Client,
    key: &str,
) -> Result<Vec<Value>, OpError> {
    let mut out: Vec<Value> = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();
    let mut offset = 0usize;
    loop {
        let page = get(client, key, &format!("/v1/receipts?limit=200&offset={offset}"))?
            .get("receipts")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        if page.is_empty() {
            break;
        }
        let (fresh, stop) = page_step(&page, &mut seen_ids);
        if fresh.is_empty() {
            break; // endpoint ignores offset: same page again, stop
        }
        out.extend(fresh);
        if stop || page.len() < 50 {
            break;
        }
        offset += page.len();
        if offset > 5000 {
            break; // sanity bound
        }
    }
    // de-dupe by receipt id (pages may overlap)
    let mut seen: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    for r in out {
        if let Some(id) = r.get("id").and_then(|i| i.as_str()) {
            seen.insert(id.to_string(), r);
        }
    }
    Ok(seen.into_values().collect())
}

pub fn parse_ts(s: Option<&str>) -> Option<DateTime<Utc>> {
    let s = s?;
    let norm = s.replace('Z', "+00:00");
    DateTime::parse_from_rfc3339(&norm).ok().map(|d| d.with_timezone(&Utc))
}

/// True iff any trace entry has gate == "VERIFY" and pass truthy.
/// (Python: `t.get("gate") == "VERIFY" and t.get("pass")` — pass must be truthy.)
pub fn verified_from_trace(trace: &[Value]) -> bool {
    trace.iter().any(|t| {
        t.get("gate").and_then(|g| g.as_str()) == Some("VERIFY")
            && t.get("pass").map(|p| !p.is_null() && p != &Value::Bool(false)).unwrap_or(false)
    })
}

pub fn score_row(verified: bool, bytes_total: f64, latency_s: Option<f64>, rate: f64) -> f64 {
    let denom = bytes_total + rate * latency_s.unwrap_or(0.0);
    if verified && denom > 0.0 {
        1.0 / denom
    } else {
        0.0
    }
}

pub fn input_chars(client: &reqwest::blocking::Client, key: &str, job_id: &str) -> Option<usize> {
    let j = get(client, key, &format!("/v1/job/{job_id}")).ok()?;
    let job = j.get("job")?;
    let spec: Value = serde_json::from_str(job.get("spec")?.as_str()?).ok()?;
    let msgs = spec.get("input")?.get("messages")?.as_array()?;
    Some(msgs.iter().map(|m| m.get("content").and_then(|c| c.as_str()).unwrap_or("").len()).sum())
}

pub fn analyze(
    client: &reqwest::blocking::Client,
    key: &str,
    receipts: Vec<Value>,
) -> (Vec<Value>, usize) {
    let mut by_job: std::collections::HashMap<String, Vec<Value>> = std::collections::HashMap::new();
    for r in receipts {
        if let Some(jid) = r.get("job_id").and_then(|j| j.as_str()) {
            by_job.entry(jid.to_string()).or_default().push(r);
        }
    }
    let mut rows = Vec::new();
    let mut skipped = 0usize;
    let mut job_ids: Vec<String> = by_job.keys().cloned().collect();
    job_ids.sort();
    for jid in job_ids {
        let mut rs = by_job.remove(&jid).unwrap();
        rs.sort_by_key(|r| {
            r.get("finished_at")
                .or_else(|| r.get("created_at"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        });
        let last = rs.last().unwrap();
        if last.get("result").and_then(|r| r.as_str()) != Some("ok") {
            continue;
        }
        let dj: Value = match serde_json::from_str(last.get("decision").and_then(|d| d.as_str()).unwrap_or("{}")) {
            Ok(v) => v,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let trace = dj.get("trace").and_then(|t| t.as_array()).cloned().unwrap_or_default();
        let verified = verified_from_trace(&trace);
        let out = dj.get("output").cloned().unwrap_or(Value::Null);
        let out_chars = out.get("content").and_then(|c| c.as_str()).unwrap_or("").len();
        let usage = out.get("usage").cloned().unwrap_or(Value::Null);
        let in_chars = match input_chars(client, key, &jid) {
            Some(n) => n,
            None => {
                skipped += 1;
                continue;
            }
        };
        let t0 = parse_ts(last.get("started_at").and_then(|v| v.as_str()));
        let t1 = parse_ts(last.get("finished_at").and_then(|v| v.as_str()));
        let latency_s = match (t0, t1) {
            (Some(a), Some(b)) => Some((b - a).num_milliseconds() as f64 / 1000.0),
            _ => None,
        };
        let cost = last.get("cost").and_then(|c| c.as_f64()).unwrap_or(0.0);
        let w = dj.get("worker");
        let model = match w {
            Some(Value::Object(_)) => w.and_then(|w| w.get("model")).and_then(|m| m.as_str()),
            Some(Value::String(s)) => Some(s.as_str()),
            _ => None,
        }
        .or_else(|| out.get("model").and_then(|m| m.as_str()))
        .unwrap_or("?")
        .to_string();
        rows.push(serde_json::json!({
            "job": jid,
            "verified": verified,
            "bytes_in": in_chars,
            "bytes_out": out_chars,
            "bytes_total": in_chars + out_chars,
            "latency_s": latency_s,
            "cost": cost,
            "tokens_in": usage.get("prompt_tokens"),
            "tokens_out": usage.get("completion_tokens"),
            "resource": last.get("resource"),
            "model": model,
        }));
    }
    (rows, skipped)
}

pub fn run(input: Value) -> Result<Value, OpError> {
    let rate = input
        .get("latency_rate")
        .and_then(|v| v.as_f64())
        .unwrap_or(LATENCY_BYTE_RATE);
    let key = exchange_key()?;
    let client = http_client(30);
    let receipts = get_all_receipts(&client, &key)?;
    let (mut rows, skipped) = analyze(&client, &key, receipts);
    for r in &mut rows {
        let bytes_total = r["bytes_total"].as_f64().unwrap_or(0.0);
        let latency_s = r["latency_s"].as_f64();
        let verified = r["verified"].as_bool().unwrap_or(false);
        r["score"] = serde_json::json!(score_row(verified, bytes_total, latency_s, rate));
    }
    rows.sort_by(|a, b| {
        b["score"]
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&a["score"].as_f64().unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(serde_json::json!({"latency_byte_rate": rate, "rows": rows, "skipped": skipped}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ts_handles_z() {
        let t = parse_ts(Some("2026-10-06T12:00:00Z")).unwrap();
        assert_eq!(t.to_rfc3339(), "2026-10-06T12:00:00+00:00");
        assert!(parse_ts(None).is_none());
        assert!(parse_ts(Some("garbage")).is_none());
    }

    #[test]
    fn verify_gate_last_entry_semantics() {
        // any() over the trace, mirroring Python's `any(...)`
        let trace = vec![
            serde_json::json!({"gate": "VERIFY", "pass": false}),
            serde_json::json!({"gate": "VERIFY", "pass": true}),
        ];
        assert!(verified_from_trace(&trace));
        let trace2 = vec![serde_json::json!({"gate": "VERIFY", "pass": false})];
        assert!(!verified_from_trace(&trace2));
        let trace3 = vec![serde_json::json!({"gate": "VERIFY"})]; // missing pass -> falsy
        assert!(!verified_from_trace(&trace3));
    }

    #[test]
    fn score_formula() {
        assert!((score_row(true, 1000.0, Some(2.0), 1000.0) - 1.0 / 3000.0).abs() < 1e-12);
        assert_eq!(score_row(false, 1000.0, Some(2.0), 1000.0), 0.0);
        assert_eq!(score_row(true, 0.0, None, 1000.0), 0.0);
        assert!((score_row(true, 500.0, None, 1000.0) - 1.0 / 500.0).abs() < 1e-12);
    }

    #[test]
    fn page_step_stops_on_repeat() {
        let page = vec![
            serde_json::json!({"id": "r1"}),
            serde_json::json!({"id": "r2"}),
        ];
        let mut seen = std::collections::HashSet::new();
        let (fresh, stop) = page_step(&page, &mut seen);
        assert_eq!(fresh.len(), 2);
        assert!(stop); // len < 50
        let (fresh2, _) = page_step(&page, &mut seen);
        assert!(fresh2.is_empty()); // same page again -> no fresh
    }
}
