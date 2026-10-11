//! ek/claim — Worker Protocol v1 claimant for lazy artifact minting.
//! Port of mint-claimant.py.
//!
//! PICKUP -> POST /v1/claim on res_localswarm (worker-scoped auth)
//! VALIDATE -> delegation fields present, mint job check
//! EXECUTE -> ew_execute::mint() IN-PROCESS (pure deterministic hash check)
//! CAPTURE -> validator result to exactly one terminal outcome
//! REPORT -> POST /v1/complete
//!
//! Terminal mapping:
//!   exit 0 -> SUCCEEDED + output.ternary_verdict
//!   exit 2 / task_error permanent (malformed task) -> FAILED_PERMANENT
//!   (timeout/crash can't happen in-process; treated FAILED_RETRYABLE)

use serde_json::Value;

use super::{ew_execute, expanduser, http_client, OpError};

pub const EXCHANGE: &str = "https://lucidota-exchange.cutecity.workers.dev";
pub const RESOURCE: &str = "res_localswarm";
pub const WORKER_ID_FILE: &str = "~/.exchange_worker_swarm_01";
pub const MAX_PER_RUN: usize = 25;

fn load_cred() -> Result<(String, String), OpError> {
    let path = expanduser(WORKER_ID_FILE);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| OpError::failed(format!("read {path}: {e}")))?;
    let mut lines = text.lines();
    let wid = lines.next().unwrap_or("").trim().to_string();
    let wsec = lines.next().unwrap_or("").trim().to_string();
    if wid.is_empty() || wsec.is_empty() {
        return Err(OpError::failed(format!("{path}: need two lines (id, secret)")));
    }
    Ok((wid, wsec))
}

fn xchg(
    client: &reqwest::blocking::Client,
    wid: &str,
    wsec: &str,
    path: &str,
    body: &Value,
) -> Result<(Value, u16), OpError> {
    let resp = client
        .post(format!("{EXCHANGE}{path}"))
        .header("Content-Type", "application/json")
        .header("X-Worker-Id", wid)
        .header("X-Worker-Secret", wsec)
        .header("User-Agent", "lucidota-mint-claimant/1.0")
        .body(serde_json::to_string(body).unwrap())
        .send()
        .map_err(|e| OpError::failed(format!("POST {path}: {e}")))?;
    let status = resp.status().as_u16();
    let val: Value = resp.json().unwrap_or_else(|_| serde_json::json!({"error": "bad json"}));
    Ok((val, status))
}

pub fn run(_input: Value) -> Result<Value, OpError> {
    let (wid, wsec) = load_cred()?;
    let client = http_client(60);
    let mut done = 0usize;
    let mut results: Vec<Value> = Vec::new();

    for _ in 0..MAX_PER_RUN {
        let (claim, st) = xchg(&client, &wid, &wsec, "/v1/claim", &serde_json::json!({"resource_id": RESOURCE}))?;
        if st != 200 || !claim.get("claimed").map(|c| !c.is_null() && c != &Value::Bool(false)).unwrap_or(false) {
            break;
        }
        let delegation = claim.get("delegation").cloned().unwrap_or(Value::Null);
        let claim_id = delegation.get("claim_id").and_then(|c| c.as_str());
        let task_input = delegation.get("input").cloned().unwrap_or(Value::Null);
        let claim_id = match claim_id {
            Some(id) => id.to_string(),
            None => {
                results.push(serde_json::json!({"mint_id": Value::Null, "outcome": "NO-CLAIM-ID"}));
                break;
            }
        };
        // DEFENSIVE: only execute mint jobs (input.mint_id present).
        // Anything else on this resource is not mine — requeue honestly via
        // FAILED_RETRYABLE so the real claimant can pick it up.
        if task_input.get("mint_id").is_none() {
            let _ = xchg(
                &client,
                &wid,
                &wsec,
                "/v1/complete",
                &serde_json::json!({
                    "claim_id": claim_id,
                    "worker_outcome": "FAILED_RETRYABLE",
                    "output": {"error": "not a mint job — requeued"},
                    "latency_ms": 0,
                }),
            );
            results.push(serde_json::json!({
                "mint_id": Value::Null,
                "outcome": "SKIPPED-NOT-MINT",
                "job_id": claim.get("job_id"),
            }));
            continue;
        }
        let task = serde_json::json!({
            "task_type": "mint",
            "artifact_path": task_input.get("artifact_path"),
            "expected_sha256": task_input.get("expected_sha256"),
            "mint_id": task_input.get("mint_id"),
            "local_receipt_id": task_input.get("local_receipt_id"),
            "label": task_input.get("label"),
        });
        // EXECUTE natively in-process (no subprocess — Rust port).
        let (res, outcome) = ew_execute::terminal_outcome(&task);
        let verdict = res.get("ternary_verdict").cloned().unwrap_or(Value::Null);
        let output = if outcome == "SUCCEEDED" {
            serde_json::json!({"ternary_verdict": verdict, "mint": res})
        } else {
            serde_json::json!({"error": res.get("error").or_else(|| res.get("detail")).cloned().unwrap_or(Value::Null)})
        };
        let latency_ms = res
            .get("detail")
            .and_then(|d| d.get("latency_ms"))
            .and_then(|l| l.as_u64())
            .unwrap_or(0);
        let (rep, _) = xchg(
            &client,
            &wid,
            &wsec,
            "/v1/complete",
            &serde_json::json!({
                "claim_id": claim_id,
                "worker_outcome": outcome,
                "output": output,
                "latency_ms": latency_ms,
            }),
        )?;
        done += 1;
        results.push(serde_json::json!({
            "mint_id": task.get("mint_id"),
            "outcome": outcome,
            "verdict": verdict,
            "receipt": rep.get("receipt_id"),
        }));
    }

    Ok(serde_json::json!({"minted": done, "results": results}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_flag_truthiness() {
        // mirrors Python: st != 200 or not claim.get("claimed")
        let truthy = |c: &Value| c.get("claimed").map(|x| !x.is_null() && x != &Value::Bool(false)).unwrap_or(false);
        assert!(truthy(&serde_json::json!({"claimed": true})));
        assert!(!truthy(&serde_json::json!({"claimed": false})));
        assert!(!truthy(&serde_json::json!({})));
        assert!(!truthy(&serde_json::json!({"claimed": null})));
    }
}
