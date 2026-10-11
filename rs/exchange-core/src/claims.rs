//! Claim lifecycle — faithful port of worker.js v0.6.16.
//!
//! - `complete_claimed`: the full Worker Protocol v1 completion path
//!   (forgery checks → idempotency → P7 outcome validation → atomic
//!   completing flip (C3 completing_at) → lease-expiry re-check →
//!   observe/quota → RATE_LIMITED spree guard → W9 model check → INV7
//!   verification → QUARANTINED → accounting → receipt → A1/A2/A3).
//! - `expire_claim`, `sweep_expired_claims` (zombies, stuck completing,
//!   orphan leases).
//! - `create_lease`, `validate_lease`, `delegate_execution`.
//!
//! SQL strings are verbatim from worker.js. Response shapes mirror the
//! JS object literals (field order preserved where it matters).

use crate::db::{row_to_json, Db, DbError, Row, RowExt, Val};
use crate::env::Env;
use crate::observe::observe;
use crate::policy::{cost_of, policy_version, taxonomy_of};
use crate::util::{minutes_ago, now_iso, sha256hex, uid};
use serde_json::{json, Value};
use std::collections::HashMap;

/// Operation response: HTTP status + JSON body.
pub struct OpResponse {
    pub status: u16,
    pub body: Value,
}

impl OpResponse {
    pub fn ok(body: Value) -> Self {
        OpResponse { status: 200, body }
    }
    pub fn err(status: u16, error: &str) -> Self {
        OpResponse {
            status,
            body: json!({ "error": error }),
        }
    }
}

/// Authenticated worker identity (from authWorker).
#[derive(Clone, Debug)]
pub struct WorkerIdent {
    pub worker_id: String,
    pub trust_class: String,
}

/// The five terminal outcomes a worker may report. Validated BEFORE the
/// active→completing flip (P7).
pub const WORKER_OUTCOMES: &[&str] = &[
    "SUCCEEDED",
    "FAILED_RETRYABLE",
    "FAILED_PERMANENT",
    "QUARANTINED",
    "EXPIRED",
];

// --- small JS-semantics helpers -------------------------------------------

/// `b.<key>` as string (JS `String(...)` of the field, None if missing/null).
fn bs(b: &Value, key: &str) -> Option<String> {
    match b.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(x)) => Some(x.to_string()),
        _ => None,
    }
}

/// JS `Number(b.latency_ms) || 0`.
fn js_number(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.parse().unwrap_or(0.0),
        Some(Value::Bool(true)) => 1.0,
        _ => 0.0,
    }
    .max(0.0)
    .min(3600000.0)
}

/// Parse an ISO-8601 datetime to epoch millis (for lease_expiry checks).
fn parse_iso_millis(s: &str) -> i64 {
    crate::util::ts(s)
}

fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// --- completeClaimed -------------------------------------------------------

/// completeClaimed — port of worker.js (C3/P7-hardened).
///
/// `b` is the raw parsed JSON body; `worker` the authenticated worker.
/// Needs `env` for the A3 needs_evidence replan (runJob).
pub async fn complete_claimed(
    db: &dyn Db,
    b: &Value,
    worker: &WorkerIdent,
    env: &Env,
) -> Result<OpResponse, DbError> {
    // Claim lookup: explicit claim_id, or transitional lease_id fallback.
    let mut claim_id = bs(b, "claim_id").filter(|s| !s.is_empty());
    if claim_id.is_none() {
        if let Some(lid) = bs(b, "lease_id") {
            let c = db
                .query_one(
                    "SELECT claim_id FROM claims WHERE lease_id=? AND status='active'",
                    &[Val::from(lid)],
                )
                .await?;
            if let Some(r) = c {
                claim_id = r.text("claim_id");
            }
        }
    }
    let claim_id = match claim_id {
        Some(c) => c,
        None => return Ok(OpResponse::err(400, "claim_id required")),
    };
    let claim = db
        .query_one("SELECT * FROM claims WHERE claim_id=?", &[Val::from(claim_id.clone())])
        .await?;
    let claim = match claim {
        Some(c) => c,
        None => return Ok(OpResponse::err(404, "unknown claim")),
    };
    // Forgery checks: the claim belongs to exactly one worker; the worker
    // cannot rebind it to a different resource or job.
    if claim.text_or("worker_id", "") != worker.worker_id {
        return Ok(OpResponse::err(
            403,
            "claim belongs to another worker — forgery rejected",
        ));
    }
    if let Some(rid) = bs(b, "resource_id") {
        if rid != claim.text_or("resource_id", "") {
            return Ok(OpResponse::err(
                400,
                "resource_id does not match claim — forgery rejected",
            ));
        }
    }
    if let Some(jid) = bs(b, "job_id") {
        if jid != claim.text_or("job_id", "") {
            return Ok(OpResponse::err(
                400,
                "job_id does not match claim — forgery rejected",
            ));
        }
    }
    // Idempotency (W2): already-completed replays the stored summary.
    if claim.text_or("status", "") == "completed" {
        let s: Value =
            serde_json::from_str(&claim.text_or("completion_summary", "{}")).unwrap_or(json!({}));
        let mut m = s.as_object().cloned().unwrap_or_default();
        m.insert("idempotent".to_string(), Value::Bool(true));
        m.insert(
            "note".to_string(),
            Value::from("already completed — no duplicate accounting"),
        );
        m.insert("status".to_string(), Value::from(200));
        return Ok(OpResponse {
            status: 200,
            body: Value::Object(m),
        });
    }
    if claim.text_or("status", "") == "expired" {
        return Ok(OpResponse::err(409, "claim expired — completion rejected"));
    }
    // P7: validate worker_outcome BEFORE the flip.
    let outcome = bs(b, "worker_outcome").unwrap_or_default();
    if !WORKER_OUTCOMES.contains(&outcome.as_str()) {
        return Ok(OpResponse::err(
            400,
            &format!("worker_outcome must be one of {}", WORKER_OUTCOMES.join(" | ")),
        ));
    }
    // ATOMIC FLIP (W2 + C3): active→completing, stamping completing_at in
    // SQLite datetime('now') format (minutesAgo(0)) — NOT ISO.
    let flip = db
        .exec(
            "UPDATE claims SET status='completing', completing_at=? WHERE claim_id=? AND status='active'",
            &[Val::from(minutes_ago(0)), Val::from(claim_id.clone())],
        )
        .await?;
    if flip == 0 {
        let done = db
            .query_one(
                "SELECT status, completion_summary FROM claims WHERE claim_id=?",
                &[Val::from(claim_id.clone())],
            )
            .await?;
        if let Some(d) = done {
            if d.text_or("status", "") == "completed" {
                let s: Value =
                    serde_json::from_str(&d.text_or("completion_summary", "{}")).unwrap_or(json!({}));
                let mut m = s.as_object().cloned().unwrap_or_default();
                m.insert("idempotent".to_string(), Value::Bool(true));
                m.insert(
                    "note".to_string(),
                    Value::from("already completed — no duplicate accounting"),
                );
                m.insert("status".to_string(), Value::from(200));
                return Ok(OpResponse {
                    status: 200,
                    body: Value::Object(m),
                });
            }
            return Ok(OpResponse::err(
                409,
                &format!(
                    "claim not active (status={}) — completion rejected",
                    d.text_or("status", "unknown")
                ),
            ));
        }
        return Ok(OpResponse::err(
            409,
            "claim not active (status=unknown) — completion rejected",
        ));
    }
    // Lease expiry re-check (W4): a worker that outran its lease does not
    // get to resurrect the job.
    if parse_iso_millis(&claim.text_or("lease_expiry", "")) < now_millis() {
        expire_claim(db, &claim, "worker completed after lease expiry").await?;
        let mut body = json!({ "error": "LEASE_EXPIRED — completion rejected, job replanned" });
        body["worker_outcome"] = Value::from("EXPIRED");
        return Ok(OpResponse { status: 409, body });
    }

    let lease_id = claim.text_or("lease_id", "");
    let ex = db
        .query_one("SELECT * FROM executions WHERE claim_id=?", &[Val::from(claim_id.clone())])
        .await?;
    let ex = match ex {
        Some(e) => e,
        None => return Ok(OpResponse::err(500, "execution row missing for claim")),
    };
    let job = db
        .query_one(
            "SELECT * FROM jobs WHERE id=?",
            &[Val::from(claim.text_or("job_id", ""))],
        )
        .await?;
    let job = match job {
        Some(j) => j,
        None => return Ok(OpResponse::err(500, "job row missing for claim")),
    };
    let mut resource = db
        .query_one(
            "SELECT * FROM resources WHERE id=?",
            &[Val::from(claim.text_or("resource_id", ""))],
        )
        .await?
        .unwrap_or_default();
    let provider = db
        .query_one(
            "SELECT * FROM providers WHERE id=?",
            &[Val::from(resource.text_or("provider_id", ""))],
        )
        .await?
        .unwrap_or_default();
    let spec: Value = serde_json::from_str(&job.text_or("spec", "{}")).unwrap_or(json!({}));
    let policy_v = policy_version(db).await?;
    let error_class = bs(b, "error_class");
    let lat = js_number(b.get("latency_ms"));
    let succeeded = outcome == "SUCCEEDED";

    // Governor-side outcome mapping.
    let taxonomy: &str = if outcome == "SUCCEEDED" {
        "ok"
    } else if outcome == "FAILED_RETRYABLE" {
        "RETRYABLE_FAILURE"
    } else if outcome == "FAILED_PERMANENT" {
        taxonomy_of(error_class.as_deref().unwrap_or("UNKNOWN"))
    } else if outcome == "QUARANTINED" {
        "PROVIDER_FAILURE"
    } else {
        "RETRYABLE_FAILURE" // EXPIRED is time-bound
    };

    let q0 = db
        .query_one(
            "SELECT used_window FROM quotas WHERE resource_id=?",
            &[Val::from(resource.text_or("id", ""))],
        )
        .await?;
    let health_before = resource.text_or("state", "");
    let quota_before = q0.map(|r| r.i64_or("used_window", 0)).unwrap_or(0);
    let obs_detail = if succeeded {
        "execution ok".to_string()
    } else {
        format!(
            "{}{}: {}",
            outcome,
            error_class.as_ref().map(|c| format!("/{}", c)).unwrap_or_default(),
            bs(b, "error").unwrap_or_default()
        )
    };
    let mut detail_str = format!("claimant: {}", obs_detail);
    detail_str.truncate(180 + "claimant: ".len());
    observe(db, &resource.text_or("id", ""), succeeded, lat, &detail_str).await?;
    crate::govern::quota_consume(db, &resource.text_or("id", "")).await?;
    if succeeded {
        db.exec(
            "UPDATE resources SET cost_basis='measured' WHERE id=?",
            &[Val::from(resource.text_or("id", ""))],
        )
        .await?;
    }

    // SPREE FIX: corroboration before throttling on RATE_LIMITED.
    if error_class.as_deref() == Some("RATE_LIMITED") {
        let recent = db
            .query_all(
                "SELECT detail FROM health_observations WHERE resource_id=? ORDER BY observed_at DESC LIMIT 5",
                &[Val::from(resource.text_or("id", ""))],
            )
            .await?;
        let mut consec_worker = 0;
        for h in &recent {
            let d = h.text_or("detail", "");
            if d.starts_with("claimant:") && d.contains("RATE_LIMITED") {
                consec_worker += 1;
            } else {
                break;
            }
        }
        let governor_seen = recent.iter().any(|h| {
            let d = h.text_or("detail", "");
            !d.starts_with("claimant:") && d.contains("RATE_LIMITED")
        });
        if consec_worker >= 2 || governor_seen {
            let w = db
                .query_one(
                    "SELECT used_window FROM quotas WHERE resource_id=?",
                    &[Val::from(resource.text_or("id", ""))],
                )
                .await?;
            let ceil = (w.map(|r| r.i64_or("used_window", 0)).unwrap_or(0)).max(1);
            let has = db
                .query_one(
                    "SELECT resource_id FROM quotas WHERE resource_id=?",
                    &[Val::from(resource.text_or("id", ""))],
                )
                .await?;
            if has.is_some() {
                db.exec(
                    "UPDATE quotas SET limit_per_min=?, measured=1, measured_at=? WHERE resource_id=?",
                    &[Val::from(ceil), Val::from(now_iso()), Val::from(resource.text_or("id", ""))],
                )
                .await?;
            } else {
                db.exec(
                    "INSERT INTO quotas (resource_id, limit_per_min, measured, measured_at) VALUES (?,?,1,?)",
                    &[Val::from(ceil), Val::from(now_iso()), Val::from(resource.text_or("id", ""))],
                )
                .await?;
            }
        }
    }

    // W9: requested vs actually-executed capacity.
    let rconf: Value =
        serde_json::from_str(&resource.text_or("config", "{}")).unwrap_or(json!({}));
    let requested_model = rconf.get("model").and_then(|v| v.as_str()).map(|s| s.to_string());
    let actual_model = bs(b, "model_actual");
    let model_match: Option<i64> = match (&requested_model, &actual_model) {
        (Some(r), Some(a)) => Some(if r == a { 1 } else { 0 }),
        _ => None,
    };
    if model_match == Some(0) {
        db.exec(
            "INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)",
            &[
                Val::from(resource.text_or("id", "")),
                Val::from(resource.text_or("evidence_state", "")),
                Val::from(resource.text_or("evidence_state", "")),
                Val::from("model-mismatch"),
                Val::from(format!(
                    "requested {}, provider executed {}",
                    requested_model.clone().unwrap_or_default(),
                    actual_model.clone().unwrap_or_default()
                )),
            ],
        )
        .await?;
    }

    // INV7: SUCCEEDED with empty content is not success.
    let mut verified = true;
    let mut verify_note: Option<String> = None;
    let mut output_hash: Option<String> = None;
    if succeeded {
        let output = b.get("output");
        let content = output.and_then(|o| o.get("content"));
        let is_inference = spec.get("capability").and_then(|v| v.as_str()).unwrap_or("") == "inference";
        let output_has_substance = match output {
            None => false,
            Some(Value::Null) => false,
            Some(Value::Object(m)) => {
                !m.is_empty()
                    && match content {
                        None => true,
                        Some(Value::String(s)) => !s.is_empty(),
                        Some(_) => false,
                    }
            }
            Some(_) => match content {
                None => true,
                Some(Value::String(s)) => !s.is_empty(),
                Some(_) => false,
            },
        };
        verified = if is_inference {
            matches!(content, Some(Value::String(s)) if !s.is_empty())
        } else {
            output_has_substance
        };
        if !verified {
            verify_note = Some(
                if is_inference {
                    "worker reported SUCCEEDED with empty/missing content for inference".to_string()
                } else {
                    "worker reported SUCCEEDED with empty output (INV7)".to_string()
                },
            );
        } else {
            output_hash = Some(sha256hex(&serde_json::to_string(b.get("output").unwrap_or(&Value::Null)).unwrap_or_default()));
        }
    }
    let final_taxonomy = if succeeded {
        if verified { "ok" } else { "VERIFICATION_FAILURE" }
    } else {
        taxonomy
    };
    let failed = !succeeded || !verified;

    // QUARANTINED is a worker signal; quarantine is the governor's decision.
    if outcome == "QUARANTINED"
        && (resource.text_or("state", "") == "DEGRADED" || resource.text_or("state", "") == "THROTTLED")
    {
        db.exec(
            "UPDATE resources SET state='QUARANTINED' WHERE id=?",
            &[Val::from(resource.text_or("id", ""))],
        )
        .await?;
        let err120: String = bs(b, "error").unwrap_or_default().chars().take(120).collect();
        db.exec(
            "INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)",
            &[
                Val::from(resource.text_or("id", "")),
                Val::from(format!("health:{}", health_before)),
                Val::from("health:QUARANTINED"),
                Val::from("worker-quarantine-signal"),
                Val::from(format!("worker {} reported QUARANTINED: {}", worker.worker_id, err120)),
            ],
        )
        .await?;
    }

    let q2 = db
        .query_one(
            "SELECT used_window FROM quotas WHERE resource_id=?",
            &[Val::from(resource.text_or("id", ""))],
        )
        .await?;
    resource = db
        .query_one(
            "SELECT * FROM resources WHERE id=?",
            &[Val::from(resource.text_or("id", ""))],
        )
        .await?
        .unwrap_or_default();
    let quota_after = q2.map(|r| r.i64_or("used_window", 0)).unwrap_or(0);

    // ACCOUNTING — the governor's books (W6).
    let measured_cost = cost_of(&resource);
    let cost_basis = resource.text_or("cost_basis", "unknown");
    let accounting_id = uid("acct");
    db.exec(
        "INSERT INTO accounting (id,execution_id,claim_id,job_id,resource_id,measured_cost,cost_basis,quota_effect) VALUES (?,?,?,?,?,?,?,?)",
        &[
            Val::from(accounting_id),
            Val::from(ex.text_or("id", "")),
            Val::from(claim_id.clone()),
            Val::from(job.text_or("id", "")),
            Val::from(resource.text_or("id", "")),
            Val::from(measured_cost),
            Val::from(cost_basis.clone()),
            Val::from(json!({ "quota_before": quota_before, "quota_after": quota_after }).to_string()),
        ],
    )
    .await?;

    let usage_json = b
        .get("output")
        .and_then(|o| o.get("usage"))
        .map(|u| serde_json::to_string(u).unwrap_or_default());
    let err_text = bs(b, "error");
    db.exec(
        "UPDATE executions SET result=?, output_hash=?, error=?, error_class=?, failure_taxonomy=?, finished_at=?, requested_model=?, actual_model=?, model_match=?, usage_json=? WHERE id=?",
        &[
            Val::from(if failed { "error" } else { "ok" }),
            output_hash.clone().map(Val::from).unwrap_or(Val::Null),
            failed.then(|| verify_note.clone().or(err_text.clone()).unwrap_or_default()).map(Val::from).unwrap_or(Val::Null),
            if failed {
                if verify_note.is_some() { Val::from("VERIFICATION_FAILED") }
                else { error_class.clone().map(Val::from).unwrap_or(Val::Null) }
            } else { Val::Null },
            Val::from(final_taxonomy),
            Val::from(now_iso()),
            requested_model.clone().map(Val::from).unwrap_or(Val::Null),
            actual_model.clone().map(Val::from).unwrap_or(Val::Null),
            model_match.map(Val::from).unwrap_or(Val::Null),
            usage_json.map(Val::from).unwrap_or(Val::Null),
            Val::from(ex.text_or("id", "")),
        ],
    )
    .await?;
    db.exec(
        "UPDATE leases SET state=? WHERE id=?",
        &[Val::from(if failed { "released" } else { "consumed" }), Val::from(lease_id.clone())],
    )
    .await?;

    // Terminal state.
    let defer_count = job.i64_or("defer_count", 0);
    let retryable = failed && (final_taxonomy == "RETRYABLE_FAILURE" || final_taxonomy == "QUOTA_FAILURE");
    let state = if !failed {
        "done"
    } else if retryable && defer_count < 8 {
        "deferred"
    } else {
        "failed"
    };
    if failed && state == "deferred" {
        db.exec(
            "UPDATE jobs SET defer_count=defer_count+1 WHERE id=?",
            &[Val::from(job.text_or("id", ""))],
        )
        .await?;
    }
    db.exec(
        "UPDATE jobs SET state=? WHERE id=?",
        &[Val::from(state), Val::from(job.text_or("id", ""))],
    )
    .await?;

    let lease_snap = db
        .query_one("SELECT * FROM leases WHERE id=?", &[Val::from(lease_id.clone())])
        .await?
        .unwrap_or_default();
    let receipt_id = uid("rcpt");
    let policy_snapshot: Value =
        serde_json::from_str(&claim.text_or("policy_snapshot", "{}")).unwrap_or(json!({}));
    let oh16 = output_hash.as_ref().map(|h| format!("{}…", &h[..h.len().min(16)]));
    let completion_trace = json!([
        { "gate": "CLAIM", "pass": true, "reason": format!("claim {} by worker {}", claim_id, worker.worker_id),
          "detail": { "lease": lease_id, "policy_snapshot": policy_snapshot }, "at": now_iso() },
        { "gate": "VERIFY", "pass": !failed,
          "reason": if failed {
              verify_note.clone().unwrap_or_else(|| format!("{}: {}", final_taxonomy, err_text.clone().unwrap_or_default()))
          } else { "worker output independently verified".to_string() },
          "detail": { "output_hash": oh16,
            "requested_model": requested_model, "actual_model": actual_model,
            "model_match": match model_match { None => Value::from("unknown"), Some(m) => Value::from(m == 1) } },
          "at": now_iso() },
        { "gate": "ACCOUNT", "pass": true, "reason": format!("job {}, lease {}", state, if failed { "released" } else { "consumed" }),
          "detail": { "taxonomy": final_taxonomy, "quota_before": quota_before, "quota_after": quota_after,
            "measured_cost": measured_cost, "cost_basis": cost_basis,
            "worker_cost_assertion": "ignored — governor's books" }, "at": now_iso() },
    ]);
    let vote_model = spec.get("input").and_then(|i| i.get("model")).cloned().unwrap_or(Value::Null);
    let decision = json!({
        "trace": completion_trace, "delegated": true, "worker": worker.worker_id,
        "vote_model": vote_model,
        "output": if !failed { b.get("output").cloned().unwrap_or(Value::Null) } else { Value::Null },
    });
    let fallback_chain = json!([{
        "attempt": ex.text_or("id", ""), "claim": claim_id, "resource": resource.text_or("id", ""),
        "lease": lease_id, "executed": true, "taxonomy": final_taxonomy, "worker_outcome": outcome,
        "error_class": if failed {
            if verify_note.is_some() { Some("VERIFICATION_FAILED".to_string()) } else { error_class.clone() }
        } else { None },
        "error": if failed { verify_note.clone().or(err_text.clone()) } else { None },
    }]);
    let errors_text = if failed {
        Some(format!("{} [{}]: {}", resource.text_or("id", ""), final_taxonomy,
            verify_note.clone().or(err_text.clone()).unwrap_or_default()))
    } else { None };
    db.exec(
        "INSERT INTO receipts (id,job_id,execution_id,attempt_id,provider,resource,started_at,finished_at,input_hash,output_hash,result,cost,cost_before,cost_after,quota_before,quota_after,health_before,health_after,policy_version,lease,decision,fallback_chain,errors,claim_id,worker_outcome) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        &[
            Val::from(receipt_id.clone()),
            Val::from(job.text_or("id", "")),
            Val::from(ex.text_or("id", "")),
            Val::from(ex.text_or("id", "")),
            Val::from(provider.text_or("id", "")),
            Val::from(resource.text_or("id", "")),
            Val::from(ex.text_or("started_at", "")),
            Val::from(now_iso()),
            Val::from(ex.text_or("request_hash", "")),
            output_hash.clone().map(Val::from).unwrap_or(Val::Null),
            Val::from(if state == "done" { "ok" } else { state }),
            Val::from(measured_cost),
            Val::from(0i64),
            Val::from(measured_cost),
            Val::from(quota_before.to_string()),
            Val::from(quota_after.to_string()),
            Val::from(health_before),
            Val::from(resource.text_or("state", "")),
            Val::from(policy_v.clone()),
            Val::from(row_to_json(&lease_snap).to_string()),
            Val::from(decision.to_string()),
            Val::from(fallback_chain.to_string()),
            errors_text.map(Val::from).unwrap_or(Val::Null),
            Val::from(claim_id.clone()),
            Val::from(outcome.clone()),
        ],
    )
    .await?;

    // A1: governor-side ternary extraction.
    let mut ternary_verdict: Option<i64> = None;
    if !failed {
        if let Some(tv) = b.get("output").and_then(|o| o.get("ternary_verdict")).and_then(|v| v.as_i64()) {
            if tv == 1 || tv == 0 || tv == -1 {
                ternary_verdict = Some(tv);
                db.exec(
                    "UPDATE receipts SET ternary_verdict=? WHERE id=?",
                    &[Val::from(tv), Val::from(receipt_id.clone())],
                )
                .await?;
            }
        }
    }
    // Voting A2: stamp ballot_id on ANY successful vote-job receipt.
    if !failed {
        if let Some(bid) = spec.get("ballot_id").and_then(|v| v.as_str()) {
            db.exec(
                "UPDATE receipts SET ballot_id=? WHERE id=?",
                &[Val::from(bid.to_string()), Val::from(receipt_id.clone())],
            )
            .await?;
        }
    }
    // A2: auto-prune on -1 (with distinct-eligible-worker corroboration).
    if ternary_verdict == Some(-1) {
        apply_disproof_prune(db, &job, &ex, &claim_id, worker, &policy_v).await?;
    }
    // A3: governor-minted needs_evidence on verdict 0.
    if ternary_verdict == Some(0) && !failed {
        let ev_depth = spec.get("evidence_depth").and_then(|v| v.as_i64()).unwrap_or(0);
        let organelle = spec.get("organelle").and_then(|v| v.as_str()).unwrap_or("");
        if organelle != "exchange-governor" && ev_depth < 2 {
            let capability = b
                .get("output")
                .and_then(|o| o.get("needs_evidence_capability"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| spec.get("capability").and_then(|v| v.as_str()).map(|s| s.to_string()))
                .unwrap_or_else(|| "cognitive".to_string());
            let gap = b
                .get("output")
                .and_then(|o| o.get("evidence_gap"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown verdict — evidence required");
            let ev_spec = json!({
                "organelle": "exchange-governor",
                "capability": capability,
                "input": { "needs_evidence_for": job.text_or("id", ""), "claim_id": claim_id,
                    "gap": gap, "evidence_depth": ev_depth + 1 },
                "data_class": spec.get("data_class").and_then(|v| v.as_str()).unwrap_or("public"),
                "max_cost": 0, "priority": 7,
                "evidence_depth": ev_depth + 1,
            });
            let ev_id = uid("job");
            let ev_hash = sha256hex(&serde_json::to_string(&ev_spec["input"]).unwrap_or_default());
            db.exec(
                "INSERT INTO jobs (id,organelle,capability,input_hash,spec,data_class,max_cost,priority,state,defer_count) VALUES (?,?,?,?,?,?,?,?,?,?)",
                &[
                    Val::from(ev_id.clone()),
                    Val::from("exchange-governor"),
                    Val::from(capability),
                    Val::from(ev_hash),
                    Val::from(ev_spec.to_string()),
                    Val::from(ev_spec["data_class"].as_str().unwrap_or("public").to_string()),
                    Val::from(0i64),
                    Val::from(7i64),
                    Val::from("submitted"),
                    Val::from(0i64),
                ],
            )
            .await?;
            let ev_res = crate::govern::run_job(db, &ev_id, &ev_spec, env, false).await?;
            let ev_state = ev_res.get("state").and_then(|v| v.as_str()).unwrap_or("?");
            let ev_err = ev_res.get("error").and_then(|v| v.as_str()).unwrap_or("?");
            db.exec(
                "INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)",
                &[
                    Val::from(format!("job:{}", job.text_or("id", ""))),
                    Val::from(state),
                    Val::from(state),
                    Val::from("needs-evidence"),
                    Val::from(format!("verdict 0 minted evidence job {} -> {}", ev_id,
                        ev_res.get("state").and_then(|v| v.as_str()).unwrap_or(ev_err))),
                ],
            )
            .await?;
            let _ = ev_state;
        }
    }

    let summary = json!({
        "job_id": job.text_or("id", ""), "state": state, "receipt_id": receipt_id,
        "lease_id": lease_id, "claim_id": claim_id, "resource": resource.text_or("id", ""),
        "taxonomy": final_taxonomy, "worker_outcome": outcome, "ternary_verdict": ternary_verdict,
        "output": if !failed { b.get("output").cloned().unwrap_or(Value::Null) } else { Value::Null },
        "error": if failed { verify_note.or(err_text).map(Value::from).unwrap_or(Value::Null) } else { Value::Null },
    });
    db.exec(
        "UPDATE claims SET status='completed', worker_outcome=?, completed_at=?, completion_summary=? WHERE claim_id=? AND status='completing'",
        &[
            Val::from(outcome.clone()),
            Val::from(now_iso()),
            Val::from(summary.to_string()),
            Val::from(claim_id.clone()),
        ],
    )
    .await?;

    let mut out_map = summary.as_object().cloned().unwrap_or_default();
    out_map.insert("status".to_string(), Value::from(200));
    Ok(OpResponse { status: 200, body: Value::Object(out_map) })
}

/// A2 disproof prune: -1 verdict prunes dependents only with corroboration
/// from 2+ DISTINCT ELIGIBLE workers (P1 hardening).
async fn apply_disproof_prune(
    db: &dyn Db,
    job: &Row,
    ex: &Row,
    claim_id: &str,
    worker: &WorkerIdent,
    policy_v: &str,
) -> Result<(), DbError> {
    let job_id = job.text_or("id", "");
    let request_hash = ex.text_or("request_hash", "");
    let deps = db
        .query_all(
            "SELECT id FROM jobs WHERE state='deferred' AND id != ? AND (input_hash = ? OR json_extract(spec, '$.depends_on') = ?)",
            &[Val::from(job_id.clone()), Val::from(request_hash.clone()), Val::from(job_id.clone())],
        )
        .await?;
    let witnesses = db
        .query_one(
            "SELECT COUNT(DISTINCT c.worker_id) AS w FROM receipts r
             JOIN claims c ON c.claim_id = r.claim_id
             JOIN workers w2 ON w2.worker_id = c.worker_id AND COALESCE(w2.disabled, 0) = 0
             WHERE r.ternary_verdict = -1
               AND json_extract(r.decision, '$.disproof_proposal') = 1
               AND r.job_id IN (SELECT id FROM jobs WHERE input_hash = ?)",
            &[Val::from(request_hash.clone())],
        )
        .await?;
    let already = db
        .query_one(
            "SELECT 1 FROM receipts r JOIN claims c ON c.claim_id = r.claim_id
             WHERE r.ternary_verdict = -1
               AND json_extract(r.decision, '$.disproof_proposal') = 1
               AND c.worker_id = ?
               AND r.job_id IN (SELECT id FROM jobs WHERE input_hash = ?)
             LIMIT 1",
            &[Val::from(worker.worker_id.clone()), Val::from(request_hash.clone())],
        )
        .await?;
    let votes = witnesses.map(|w| w.i64_or("w", 0)).unwrap_or(0) + if already.is_some() { 0 } else { 1 };
    // NOTE: the current receipt's ternary_verdict was just stamped above;
    // the witnesses query counts receipts with disproof_proposal=1 in
    // decision, which this receipt does not yet have (it gets it below if
    // votes < 2). The current worker counts via the +1 above.
    let receipt_id: String = db
        .query_one(
            "SELECT id FROM receipts WHERE claim_id=? ORDER BY created_at DESC LIMIT 1",
            &[Val::from(claim_id.to_string())],
        )
        .await?
        .map(|r| r.text_or("id", ""))
        .unwrap_or_default();
    if votes >= 2 {
        // ATOMIC PRUNE: one batch; conditional dep updates; deterministic
        // prune receipt ids with ON CONFLICT DO NOTHING.
        let mut stmts: Vec<(String, Vec<Val>)> = Vec::new();
        for dep in &deps {
            let dep_id = dep.text_or("id", "");
            let prune_id = format!("rcpt-prune-{}-{}", dep_id, job_id);
            stmts.push((
                "UPDATE jobs SET state='failed' WHERE id=? AND state='deferred'".to_string(),
                vec![Val::from(dep_id.clone())],
            ));
            stmts.push((
                "UPDATE leases SET state='released' WHERE job_id=? AND state='active'".to_string(),
                vec![Val::from(dep_id.clone())],
            ));
            stmts.push((
                "INSERT INTO receipts (id,job_id,result,policy_version,decision,errors,ternary_verdict) VALUES (?,?,?,?,?,?,?) ON CONFLICT(id) DO NOTHING".to_string(),
                vec![
                    Val::from(prune_id),
                    Val::from(dep_id.clone()),
                    Val::from("failed"),
                    Val::from(policy_v.to_string()),
                    Val::from(json!({ "pruned": true, "trigger": "auto-prune",
                        "disproven_by": job_id, "claim": claim_id, "witnesses": votes,
                        "reason": "dependency disproven (-1) — O(1) prune, no replan" }).to_string()),
                    Val::from(format!("pruned: dependency {} disproven", job_id)),
                    Val::from(-1i64),
                ],
            ));
            stmts.push((
                "INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)".to_string(),
                vec![
                    Val::from(format!("job:{}", dep_id)),
                    Val::from("deferred"),
                    Val::from("failed"),
                    Val::from("auto-prune"),
                    Val::from(format!("disproven by {} (claim {}) — {} distinct witnesses", job_id, claim_id, votes)),
                ],
            ));
        }
        if !stmts.is_empty() {
            db.batch(stmts).await?;
        }
        if !deps.is_empty() {
            db.exec(
                "INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)",
                &[
                    Val::from(format!("job:{}", job_id)),
                    Val::from(job.text_or("state", "")),
                    Val::from(job.text_or("state", "")),
                    Val::from("auto-prune"),
                    Val::from(format!("verdict -1 pruned {} dependent job(s) — {} distinct witnesses", deps.len(), votes)),
                ],
            )
            .await?;
        }
    } else {
        // Single -1: record as proposal, do NOT prune.
        db.exec(
            "UPDATE receipts SET decision=json_patch(decision, ?) WHERE id=?",
            &[
                Val::from(json!({ "disproof_proposal": 1, "votes": 1, "proposer": worker.worker_id }).to_string()),
                Val::from(receipt_id),
            ],
        )
        .await?;
    }
    Ok(())
}

// --- expireClaim / sweepExpiredClaims ---------------------------------------

/// expireClaim: a claim outran its lease with no completion.
pub async fn expire_claim(db: &dyn Db, claim: &Row, reason: &str) -> Result<(), DbError> {
    let now = now_iso();
    let claim_id = claim.text_or("claim_id", "");
    let lease_id = claim.text_or("lease_id", "");
    let job_id = claim.text_or("job_id", "");
    let resource_id = claim.text_or("resource_id", "");
    db.exec(
        "UPDATE claims SET status='expired', worker_outcome='EXPIRED', completed_at=? WHERE claim_id=?",
        &[Val::from(now.clone()), Val::from(claim_id.clone())],
    )
    .await?;
    db.exec("UPDATE leases SET state='expired' WHERE id=?", &[Val::from(lease_id)]).await?;
    db.exec(
        "UPDATE executions SET result='uncertain', error=?, finished_at=? WHERE claim_id=?",
        &[
            Val::from(format!("lease expired before completion — execution uncertain: {}", reason)),
            Val::from(now.clone()),
            Val::from(claim_id.clone()),
        ],
    )
    .await?;
    let job = db
        .query_one("SELECT defer_count FROM jobs WHERE id=?", &[Val::from(job_id.clone())])
        .await?;
    let dc = job.map(|j| j.i64_or("defer_count", 0)).unwrap_or(0);
    if dc < 8 {
        db.exec(
            "UPDATE jobs SET state='deferred', defer_count=defer_count+1 WHERE id=?",
            &[Val::from(job_id.clone())],
        )
        .await?;
    } else {
        db.exec("UPDATE jobs SET state='failed' WHERE id=?", &[Val::from(job_id.clone())])
            .await?;
    }
    let trace = json!({ "trace": [{ "gate": "EXPIRE", "pass": true, "reason": reason, "at": now,
        "detail": { "claim": claim_id, "uncertainty": "provider may have executed — retries are not free" } }] });
    db.exec(
        "INSERT INTO receipts (id,job_id,execution_id,attempt_id,provider,resource,started_at,finished_at,input_hash,output_hash,result,cost,policy_version,decision,errors,claim_id,worker_outcome) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        &[
            Val::from(uid("rcpt")),
            Val::from(job_id.clone()),
            Val::Null, Val::Null, Val::Null,
            Val::from(resource_id),
            Val::from(claim.text_or("claimed_at", "")),
            Val::from(now.clone()),
            Val::from(claim.text_or("input_hash", "")),
            Val::Null,
            Val::from(if dc < 8 { "deferred" } else { "failed" }),
            Val::from(0i64),
            Val::from(policy_version(db).await?),
            Val::from(trace.to_string()),
            Val::from(format!("claim {}: {}", claim_id, reason)),
            Val::from(claim_id),
            Val::from("EXPIRED"),
        ],
    )
    .await?;
    Ok(())
}

/// sweepExpiredClaims: zombies, stuck 'completing' (C3), orphan leases.
pub async fn sweep_expired_claims(db: &dyn Db) -> Result<Vec<Value>, DbError> {
    let mut out = Vec::new();
    let rows = db
        .query_all(
            "SELECT * FROM claims WHERE status='active' AND lease_expiry < ?",
            &[Val::from(now_iso())],
        )
        .await?;
    for claim in rows {
        let cid = claim.text_or("claim_id", "");
        let jid = claim.text_or("job_id", "");
        expire_claim(db, &claim, "zombie sweep — no completion before lease expiry").await?;
        out.push(json!({ "claim": cid, "job": jid, "outcome": "EXPIRED" }));
    }
    // STUCK COMPLETIONS (C3): key on completing_at, NOT claimed_at.
    let stuck = db
        .query_all(
            "SELECT claim_id, job_id FROM claims WHERE status='completing' AND (
               completing_at < datetime('now','-15 minutes')
               OR (completing_at IS NULL AND claimed_at < datetime('now','-15 minutes'))
             ) LIMIT 10",
            &[],
        )
        .await?;
    for claim in stuck {
        let cid = claim.text_or("claim_id", "");
        let jid = claim.text_or("job_id", "");
        db.exec(
            "UPDATE claims SET status='active' WHERE claim_id=? AND status='completing'",
            &[Val::from(cid.clone())],
        )
        .await?;
        out.push(json!({ "claim": cid, "job": jid, "outcome": "RESET_COMPLETING_TO_ACTIVE" }));
    }
    // ORPHAN LEASES.
    let orphans = db
        .query_all(
            "SELECT l.* FROM leases l LEFT JOIN claims c ON c.lease_id=l.id
             WHERE l.state='claimed' AND c.claim_id IS NULL
             AND l.created_at < datetime('now','-15 minutes') LIMIT 10",
            &[],
        )
        .await?;
    for l in orphans {
        let lid = l.text_or("id", "");
        let jid = l.text_or("job_id", "");
        let rid = l.text_or("resource_id", "");
        db.exec("UPDATE leases SET state='expired' WHERE id=?", &[Val::from(lid.clone())])
            .await?;
        let job = db
            .query_one("SELECT defer_count FROM jobs WHERE id=?", &[Val::from(jid.clone())])
            .await?;
        let dc = job.map(|j| j.i64_or("defer_count", 0)).unwrap_or(0);
        if dc < 8 {
            db.exec(
                "UPDATE jobs SET state='deferred', defer_count=defer_count+1 WHERE id=?",
                &[Val::from(jid.clone())],
            )
            .await?;
        } else {
            db.exec("UPDATE jobs SET state='failed' WHERE id=?", &[Val::from(jid.clone())])
                .await?;
        }
        let trace = json!({ "trace": [{ "gate": "ORPHAN", "pass": true, "at": now_iso(),
            "reason": "lease claimed but no claim row — process died mid-claim; expired, job replanned",
            "detail": { "lease": lid } }] });
        db.exec(
            "INSERT INTO receipts (id,job_id,execution_id,provider,resource,started_at,finished_at,input_hash,result,cost,policy_version,decision,errors) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
            &[
                Val::from(uid("rcpt")),
                Val::from(jid.clone()),
                Val::Null, Val::Null,
                Val::from(rid),
                Val::from(l.text_or("created_at", "")),
                Val::from(now_iso()),
                Val::Null,
                Val::from(if dc < 8 { "deferred" } else { "failed" }),
                Val::from(0i64),
                Val::from(policy_version(db).await?),
                Val::from(trace.to_string()),
                Val::from(format!("orphan lease {}: claimed with no claim row", lid)),
            ],
        )
        .await?;
        out.push(json!({ "orphan_lease": lid, "job": jid, "outcome": "EXPIRED" }));
    }
    Ok(out)
}

// --- Leases + delegation ----------------------------------------------------

/// createLease: mint a delegation lease (~60s TTL).
pub async fn create_lease(
    db: &dyn Db,
    job_id: &str,
    resource_id: &str,
    expected_cost: f64,
    quota_snapshot: &Value,
    policy_v: &str,
) -> Result<HashMap<String, String>, DbError> {
    let lease_id = uid("lease");
    let expires_at = (chrono::Utc::now() + chrono::Duration::seconds(60))
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    db.exec(
        "INSERT INTO leases (id,job_id,resource_id,state,authority,expected_cost,quota_snapshot,policy_version,expires_at) VALUES (?,?,?,?,?,?,?,?,?)",
        &[
            Val::from(lease_id.clone()),
            Val::from(job_id.to_string()),
            Val::from(resource_id.to_string()),
            Val::from("active"),
            Val::from("bearer:exchange-key"),
            Val::from(expected_cost),
            Val::from(quota_snapshot.to_string()),
            Val::from(policy_v.to_string()),
            Val::from(expires_at.clone()),
        ],
    )
    .await?;
    let mut m = HashMap::new();
    m.insert("lease_id".to_string(), lease_id);
    m.insert("expires_at".to_string(), expires_at);
    Ok(m)
}

/// validateLease: exists, active, unexpired (marks expired on the way out).
pub async fn validate_lease(db: &dyn Db, lease_id: &str) -> Result<(bool, String, Option<Row>), DbError> {
    let l = db
        .query_one("SELECT * FROM leases WHERE id=?", &[Val::from(lease_id.to_string())])
        .await?;
    match l {
        None => Ok((false, "lost (no such lease)".to_string(), None)),
        Some(row) => {
            if row.text_or("state", "") != "active" {
                return Ok((false, format!("not active (state={})", row.text_or("state", "")), None));
            }
            if parse_iso_millis(&row.text_or("expires_at", "")) < now_millis() {
                db.exec("UPDATE leases SET state='expired' WHERE id=?", &[Val::from(lease_id.to_string())])
                    .await?;
                return Ok((false, "expired".to_string(), None));
            }
            Ok((true, String::new(), Some(row)))
        }
    }
}

/// delegateExecution: the governor reserves capacity and parks the job as
/// deferred; a worker picks it up via /v1/claim.
pub async fn delegate_execution(
    db: &dyn Db,
    job_id: &str,
    spec: &Value,
    resource: &Row,
    provider: &Row,
    trace: &mut Vec<Value>,
) -> Result<Value, DbError> {
    let policy_v = policy_version(db).await?;
    let input_hash = sha256hex(&serde_json::to_string(&spec["input"]).unwrap_or_default());
    let resource_id = resource.text_or("id", "");
    let lease = create_lease(db, job_id, &resource_id, cost_of(resource), &json!({ "delegated": true }), &policy_v).await?;
    let lease_id = lease["lease_id"].clone();
    let pickup = (chrono::Utc::now() + chrono::Duration::minutes(10))
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    db.exec("UPDATE leases SET expires_at=? WHERE id=?", &[Val::from(pickup), Val::from(lease_id.clone())]).await?;
    db.exec("UPDATE jobs SET state='deferred' WHERE id=?", &[Val::from(job_id.to_string())]).await?;
    trace.push(json!({ "gate": "LEASE", "pass": true,
        "reason": format!("lease {} delegated (pickup window 10m)", lease_id),
        "detail": { "resource": resource_id, "expected_cost": cost_of(resource), "policy_version": policy_v },
        "at": now_iso() }));
    trace.push(json!({ "gate": "EXECUTE", "pass": true, "reason": "delegated to claimant — awaiting pickup",
        "detail": { "resource": resource_id, "lease": lease_id }, "at": now_iso() }));
    let receipt_id = uid("rcpt");
    let lease_snap = db
        .query_one("SELECT * FROM leases WHERE id=?", &[Val::from(lease_id.clone())])
        .await?
        .unwrap_or_default();
    db.exec(
        "INSERT INTO receipts (id,job_id,execution_id,attempt_id,provider,resource,started_at,finished_at,input_hash,output_hash,result,cost,cost_before,cost_after,quota_before,quota_after,health_before,health_after,policy_version,lease,decision,fallback_chain,errors,worker_outcome) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        &[
            Val::from(receipt_id.clone()),
            Val::from(job_id.to_string()),
            Val::Null, Val::Null,
            Val::from(provider.text_or("id", "")),
            Val::from(resource_id.clone()),
            Val::from(now_iso()),
            Val::Null,
            Val::from(input_hash),
            Val::Null,
            Val::from("deferred"),
            Val::from(0i64),
            Val::from(cost_of(resource)),
            Val::from(0i64),
            Val::Null, Val::Null,
            Val::from(resource.text_or("state", "")),
            Val::Null,
            Val::from(policy_v),
            Val::from(row_to_json(&lease_snap).to_string()),
            Val::from(json!({ "trace": trace.clone(), "delegated": true,
                "trust_required": resource.text_or("claimant_trust_required", "any") }).to_string()),
            Val::from(json!([]).to_string()),
            Val::from("delegated to claimant — result pending"),
            Val::Null,
        ],
    )
    .await?;
    db.exec(
        "INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)",
        &[
            Val::from(resource_id.clone()),
            Val::from(resource.text_or("evidence_state", "")),
            Val::from(resource.text_or("evidence_state", "")),
            Val::from("delegated"),
            Val::from(format!("job {} awaiting claimant", job_id)),
        ],
    )
    .await?;
    Ok(json!({ "job_id": job_id, "state": "deferred", "lease_id": lease_id,
               "receipt_id": receipt_id, "trace": trace }))
}
