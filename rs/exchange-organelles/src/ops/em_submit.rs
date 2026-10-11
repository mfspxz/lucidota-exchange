//! em/submit — batch-submit mint jobs from ~/workspace/mint-queue.jsonl.
//! Port of mint-submit.py.
//!
//! Reads queued items (highest priority first), verifies the artifact file
//! exists locally, and POSTs each as a /v1/job with capability "cognitive".
//! Idempotency: idempotency_key = mint_id. Respects 429 backpressure
//! (stops the batch, keeps queue).

use serde_json::Value;

use super::{exchange_key, expanduser, http_client, OpError};

pub const EXCHANGE: &str = "https://lucidota-exchange.cutecity.workers.dev";
pub const QUEUE: &str = "~/workspace/mint-queue.jsonl";

#[derive(Debug, Clone)]
pub struct QueueItem {
    pub mint_id: String,
    pub priority: i64,
    pub status: String,
    pub artifact_path: String,
    pub sha256: String,
    pub local_receipt_id: Value,
    pub label: Value,
}

pub fn parse_queue(text: &str) -> Vec<QueueItem> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| {
            Some(QueueItem {
                mint_id: v.get("mint_id")?.as_str()?.to_string(),
                priority: v.get("priority")?.as_i64()?,
                status: v.get("status")?.as_str()?.to_string(),
                artifact_path: v.get("artifact_path")?.as_str()?.to_string(),
                sha256: v.get("sha256").and_then(|s| s.as_str()).unwrap_or("").to_string(),
                local_receipt_id: v.get("local_receipt_id").cloned().unwrap_or(Value::Null),
                label: v.get("label").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// Filter + sort the work list: queued status, optional priority pin,
/// sorted by (priority, mint_id), optional limit. Pure, testable.
pub fn work_list(items: &[QueueItem], prio: Option<i64>, limit: Option<usize>) -> Vec<QueueItem> {
    let mut todo: Vec<QueueItem> = items
        .iter()
        .filter(|x| x.status == "queued" && prio.map(|p| x.priority == p).unwrap_or(true))
        .cloned()
        .collect();
    todo.sort_by(|a, b| (a.priority, &a.mint_id).cmp(&(b.priority, &b.mint_id)));
    if let Some(n) = limit {
        if n > 0 {
            todo.truncate(n);
        }
    }
    todo
}

fn post_job(client: &reqwest::blocking::Client, key: &str, spec: &Value) -> Result<(Value, u16), OpError> {
    let resp = client
        .post(format!("{EXCHANGE}/v1/job"))
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {key}"))
        .header("User-Agent", "lucidota-mint-submit/1.0")
        .body(serde_json::to_string(spec).unwrap())
        .send()
        .map_err(|e| OpError::failed(format!("POST /v1/job: {e}")))?;
    let status = resp.status().as_u16();
    let val: Value = resp.json().unwrap_or_else(|_| serde_json::json!({"error": "bad json"}));
    Ok((val, status))
}

pub fn run(input: Value) -> Result<Value, OpError> {
    let dry = input.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false);
    let prio = input.get("priority").and_then(|v| v.as_i64());
    let limit = input.get("limit").and_then(|v| v.as_u64()).map(|n| n as usize);

    let queue_path = expanduser(QUEUE);
    let text = std::fs::read_to_string(&queue_path)
        .map_err(|e| OpError::failed(format!("read {queue_path}: {e}")))?;
    let items = parse_queue(&text);
    let todo = work_list(&items, prio, limit);

    // Pre-verify artifacts locally (no point submitting for missing files)
    let mut verified: Vec<&QueueItem> = Vec::new();
    let mut missing: Vec<&str> = Vec::new();
    for x in &todo {
        if std::path::Path::new(&x.artifact_path).is_file() {
            verified.push(x);
        } else {
            missing.push(&x.mint_id);
        }
    }

    let key = exchange_key()?;
    let client = http_client(60);
    let (mut submitted, mut deduped, mut backpressured, mut failed) = (0usize, 0usize, 0usize, 0usize);
    let mut results: Vec<Value> = Vec::new();
    for x in verified {
        if dry {
            results.push(serde_json::json!({"mint_id": &x.mint_id, "dry": true}));
            continue;
        }
        let spec = serde_json::json!({
            "organelle": "mint-worker",
            "capability": "cognitive",
            "input": {
                "task_type": "mint",
                "mint_id": &x.mint_id,
                "artifact_path": &x.artifact_path,
                "expected_sha256": &x.sha256,
                "local_receipt_id": &x.local_receipt_id,
                "label": &x.label,
            },
            "data_class": "public",
            "max_cost": 0,
            "priority": 6,
            "idempotency_key": &x.mint_id,
        });
        let (body, st) = post_job(&client, &key, &spec)?;
        if st == 429 {
            backpressured += 1;
            results.push(serde_json::json!({"mint_id": &x.mint_id, "outcome": "BACKPRESSURE-429"}));
            break; // stop the batch; queue keeps the rest
        }
        if st != 200 {
            failed += 1;
            let err: String = serde_json::to_string(&body).unwrap_or_default().chars().take(200).collect();
            results.push(serde_json::json!({"mint_id": &x.mint_id, "outcome": "FAILED", "error": err}));
            continue;
        }
        if body.get("deduplicated").and_then(|d| d.as_bool()).unwrap_or(false) {
            deduped += 1;
        } else {
            submitted += 1;
        }
        results.push(serde_json::json!({
            "mint_id": &x.mint_id, "outcome": "SUBMITTED",
            "job_id": body.get("job_id"), "state": body.get("state"),
        }));
    }

    Ok(serde_json::json!({
        "submitted": submitted, "deduped": deduped,
        "backpressured": backpressured, "failed": failed,
        "missing_artifacts": missing, "dry_run": dry,
        "results": results,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUEUE_TEXT: &str = r#"{"mint_id":"m3","priority":2,"status":"queued","artifact_path":"/a","sha256":"s","local_receipt_id":null,"label":"x"}
{"mint_id":"m1","priority":1,"status":"queued","artifact_path":"/b","sha256":"s","local_receipt_id":null,"label":"x"}
{"mint_id":"m2","priority":1,"status":"done","artifact_path":"/c","sha256":"s","local_receipt_id":null,"label":"x"}
{"mint_id":"m4","priority":1,"status":"queued","artifact_path":"/d","sha256":"s","local_receipt_id":null,"label":"x"}
"#;

    #[test]
    fn work_list_filters_sorts_limits() {
        let items = parse_queue(QUEUE_TEXT);
        assert_eq!(items.len(), 4);
        let todo = work_list(&items, None, None);
        let ids: Vec<&str> = todo.iter().map(|x| x.mint_id.as_str()).collect();
        assert_eq!(ids, vec!["m1", "m4", "m3"]); // (1,m1) (1,m4) (2,m3); m2 done
        let pinned = work_list(&items, Some(2), None);
        assert_eq!(pinned.len(), 1);
        assert_eq!(pinned[0].mint_id, "m3");
        let limited = work_list(&items, None, Some(2));
        assert_eq!(limited.len(), 2);
    }
}
