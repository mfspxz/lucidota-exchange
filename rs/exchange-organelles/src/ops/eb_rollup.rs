//! eb/rollup — ballot cost accounting rollup (read-only).
//! Port of ballot-cost-rollup.py: vote costs -> ballot_total_cost,
//! cost basis per vote, ballot receipt references job_ids.

use serde_json::Value;

use super::{exchange_key, http_client, OpError};

pub const BASE: &str = "https://lucidota-exchange.cutecity.workers.dev";
pub const UA: &str = "Mozilla/5.0 (X11; Linux x86_64)";

fn api_get(client: &reqwest::blocking::Client, key: &str, path: &str) -> Result<Value, OpError> {
    let resp = client
        .get(format!("{BASE}{path}"))
        .header("Authorization", format!("Bearer {key}"))
        .header("User-Agent", UA)
        .send()
        .map_err(|e| OpError::failed(format!("GET {path}: {e}")))?;
    resp.json::<Value>()
        .map_err(|e| OpError::failed(format!("GET {path} decode: {e}")))
}

/// Pure report assembly from fetched payloads (testable without network).
pub fn build_report(ballot_id: &str, ballot: &Value, jobs: &[(Value, Result<Value, String>)], receipts: &Value) -> Value {
    let votes = ballot
        .get("tally")
        .and_then(|t| t.get("votes"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut rows = Vec::new();
    let mut total = 0.0f64;
    for (v, job) in votes.iter().zip(jobs.iter().map(|(_, r)| r)) {
        let job_id = v.get("job_id").and_then(|j| j.as_str());
        match job_id {
            None => rows.push(serde_json::json!({
                "vote": v.get("vote"), "job_id": Value::Null,
                "cost": Value::Null, "cost_basis": Value::Null,
            })),
            Some(jid) => match job {
                Err(e) => rows.push(serde_json::json!({
                    "vote": v.get("vote"), "job_id": jid,
                    "cost": Value::Null, "cost_basis": Value::Null,
                    "error": e.chars().take(80).collect::<String>(),
                })),
                Ok(j) => {
                    let accts = j.get("accounting").and_then(|a| a.as_array()).cloned().unwrap_or_default();
                    let cost: f64 = accts
                        .iter()
                        .map(|a| a.get("measured_cost").and_then(|c| c.as_f64()).unwrap_or(0.0))
                        .sum();
                    let basis = accts.first().and_then(|a| a.get("cost_basis")).cloned().unwrap_or(Value::Null);
                    total += cost;
                    rows.push(serde_json::json!({
                        "vote": v.get("vote"), "model": v.get("model"), "job_id": jid,
                        "cost": cost, "cost_basis": basis, "valid": v.get("valid"),
                    }));
                }
            },
        }
    }
    let ballot_receipts: Vec<Value> = receipts
        .get("receipts")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|r| r.get("id").cloned())
        .collect();
    let all_ref = rows.iter().all(|r| {
        r.get("job_id").map(|j| !j.is_null()).unwrap_or(false)
    }) && !ballot_receipts.is_empty();
    serde_json::json!({
        "ballot_id": ballot_id,
        "state": ballot.get("state"),
        "verdict": ballot.get("verdict"),
        "ballot_total_cost": (total * 1e6).round() / 1e6,
        "votes": rows,
        "ballot_receipt_ids": ballot_receipts,
        "all_job_ids_referenced": all_ref,
    })
}

pub fn run(input: Value) -> Result<Value, OpError> {
    let ballot_id = input
        .get("ballot_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| OpError::rejected("ballot_id required"))?;
    let key = exchange_key()?;
    let client = http_client(30);

    let ballot = api_get(&client, &key, &format!("/v1/ballots/{ballot_id}"))?;
    let votes = ballot
        .get("tally")
        .and_then(|t| t.get("votes"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut jobs = Vec::new();
    for v in &votes {
        match v.get("job_id").and_then(|j| j.as_str()) {
            None => jobs.push((v.clone(), Err("no job_id".to_string()))),
            Some(jid) => {
                let r = api_get(&client, &key, &format!("/v1/job/{jid}"))
                    .map_err(|e| e.to_string());
                jobs.push((v.clone(), r));
            }
        }
    }
    let receipts = api_get(&client, &key, &format!("/v1/receipts?job=ballot:{ballot_id}"))?;
    Ok(build_report(ballot_id, &ballot, &jobs, &receipts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_sums_costs_and_rounds() {
        let ballot = serde_json::json!({
            "state": "decided", "verdict": "ok",
            "tally": {"votes": [
                {"vote": "+1", "model": "m", "job_id": "j1", "valid": true},
                {"vote": "-1", "model": "m", "job_id": "j2", "valid": true},
                {"vote": "+1", "model": "m", "valid": true},
            ]},
        });
        let jobs = vec![
            (
                serde_json::json!({}),
                Ok(serde_json::json!({"accounting": [
                    {"measured_cost": 0.123456789, "cost_basis": "per-token"},
                    {"measured_cost": 0.1},
                ]})),
            ),
            (
                serde_json::json!({}),
                Err("connection refused".to_string()),
            ),
            (serde_json::json!({}), Err("no job_id".to_string())),
        ];
        let receipts = serde_json::json!({"receipts": [{"id": "rc1"}, {"id": "rc2"}]});
        let rep = build_report("b1", &ballot, &jobs, &receipts);
        assert_eq!(rep["ballot_total_cost"], 0.223457); // round(0.223456789, 6)
        assert_eq!(rep["votes"][0]["cost"], 0.223456789);
        assert_eq!(rep["votes"][0]["cost_basis"], "per-token");
        assert!(rep["votes"][1].get("error").is_some());
        assert_eq!(rep["votes"][2]["job_id"], Value::Null);
        assert_eq!(rep["ballot_receipt_ids"], serde_json::json!(["rc1", "rc2"]));
        assert_eq!(rep["all_job_ids_referenced"], false); // one row has null job_id
    }

    #[test]
    fn missing_ballot_id_rejected() {
        assert!(matches!(run(serde_json::json!({})), Err(OpError { verdict: "rejected", .. })));
    }
}
