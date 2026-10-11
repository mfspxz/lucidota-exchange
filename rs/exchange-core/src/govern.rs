//! Governor: validateJob, govern (10 hard gates), submitJob, runJob,
//! quotaConsume, unclaimedQueueDepth — port of worker.js.
//!
//! Gate order: IDENTITY → AUTHORITY → DATA → CAPABILITY → EVIDENCE →
//! QUOTA → COST → INTEGRITY → (EFFICIENCY scores; scoring ≠ auth) →
//! LEASE → EXECUTE. Every gate recorded; no bypass.

use crate::adapters::adapter_for;
use crate::claims::{create_lease, delegate_execution, validate_lease};
use crate::db::{Db, DbError, RowExt, Val};
use crate::env::Env;
use crate::observe::{observe, stale_after_ev, EVIDENCE_USABLE};
use crate::policy::{cost_of, policy_version, taxonomy_of};
use crate::util::{minutes_ago, now_iso, sha256hex, ts, uid};
use serde_json::{json, Value};

pub const QUEUE_FULL_THRESHOLD: i64 = 40;

pub struct GovernResult {
    pub trace: Vec<Value>,
    pub selected: Option<Value>,
    pub rejected: Vec<Value>,
    pub ranked: Vec<Value>,
}

/// `validateJob(spec)` — returns a list of error strings (empty = valid).
pub fn validate_job(spec: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    let obj = match spec.as_object() {
        Some(o) => o,
        None => return vec!["job must be an object".to_string()],
    };
    if obj.get("capability").and_then(|v| v.as_str()).unwrap_or("").is_empty() {
        errs.push("capability required".to_string());
    }
    if obj.get("input").is_none() {
        errs.push("input required".to_string());
    }
    let dc = obj.get("data_class").and_then(|v| v.as_str()).unwrap_or("public");
    if !["public", "internal", "sensitive", "secret"].contains(&dc) {
        errs.push("bad data_class".to_string());
    }
    let max_cost = obj.get("max_cost").and_then(|v| v.as_f64()).unwrap_or(0.0);
    if obj.get("max_cost").map(|v| !v.is_number()).unwrap_or(false) || max_cost < 0.0 {
        errs.push("max_cost must be >= 0".to_string());
    }
    if let Some(sa) = obj.get("spend_authority") {
        if sa.as_f64().map(|n| n < 0.0).unwrap_or(true) {
            errs.push("spend_authority must be >= 0".to_string());
        }
    }
    errs
}

fn gate(trace: &mut Vec<Value>, name: &str, pass: bool, reason: &str, detail: Value) {
    trace.push(json!({ "gate": name, "pass": pass, "reason": reason,
                       "detail": detail, "at": now_iso() }));
}

/// govern: the 10 hard gates over candidate resources.
pub async fn govern(db: &dyn Db, spec: &Value, env: &Env) -> Result<GovernResult, DbError> {
    let mut trace: Vec<Value> = Vec::new();
    let mut rejected: Vec<Value> = Vec::new();
    let gate_order = ["IDENTITY", "AUTHORITY", "DATA", "CAPABILITY", "EVIDENCE", "QUOTA", "COST", "INTEGRITY", "EFFICIENCY"];
    // AUTHORITY (job-level) — bearer key already validated by authed(); record it.
    gate(&mut trace, "AUTHORITY", true, "bearer key valid", Value::Null);

    // CAPABILITY — candidate resources exposing the capability.
    let capability = spec.get("capability").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let cands = db
        .query_all(
            "SELECT r.* FROM resources r JOIN resource_capabilities c ON c.resource_id=r.id WHERE c.capability=?",
            &[Val::from(capability.clone())],
        )
        .await?;
    let mut candidates = cands;
    gate(
        &mut trace,
        "CAPABILITY",
        !candidates.is_empty(),
        &if !candidates.is_empty() {
            format!("{} resource(s) expose {}", candidates.len(), capability)
        } else {
            format!("no resource exposes {}", capability)
        },
        json!({ "candidates": candidates.iter().map(|c| c.text_or("id", "")).collect::<Vec<_>>() }),
    );
    if candidates.is_empty() {
        vacuous(&mut trace, &gate_order);
        return Ok(GovernResult { trace, selected: None, rejected, ranked: vec![] });
    }

    // EVIDENCE — promotional or hypothetical capacity is not usable capacity.
    let mut usable = Vec::new();
    for r in candidates {
        let ev = r.text_or("evidence_state", "UNKNOWN");
        if EVIDENCE_USABLE.contains(&ev.as_str()) {
            usable.push(r);
        } else {
            rejected.push(json!({ "resource": r.text_or("id", ""),
                "gate": "EVIDENCE",
                "reason": format!("evidence_state={} — not verified usable capacity", ev),
                "taxonomy": "CAPABILITY_FAILURE", "retryable": false }));
        }
    }
    gate(
        &mut trace,
        "EVIDENCE",
        !usable.is_empty(),
        &if !usable.is_empty() {
            format!("{} resource(s) with verified usable capacity", usable.len())
        } else {
            "no candidate has verified usable capacity — listings and trials do not execute".to_string()
        },
        json!({
            "usable": usable.iter().map(|r| r.text_or("id", "")).collect::<Vec<_>>(),
            "refused": rejected.iter().filter(|x| x["gate"] == "EVIDENCE").map(|x| x["resource"].clone()).collect::<Vec<_>>(),
        }),
    );
    candidates = usable;
    if candidates.is_empty() {
        vacuous(&mut trace, &gate_order);
        return Ok(GovernResult { trace, selected: None, rejected, ranked: vec![] });
    }

    let policies = db.query_all("SELECT * FROM policies", &[]).await?;
    // survivors: (row, cfg, quota, adapter, effective_cost)
    struct Survivor {
        id: String,
        cfg: Value,
        quota: Option<crate::db::Row>,
        effective_cost: f64,
    }
    let mut survivors: Vec<(crate::db::Row, Survivor)> = Vec::new();

    for r in candidates {
        let rid = r.text_or("id", "");
        let cfg: Option<Value> = serde_json::from_str(&r.text_or("config", "{}")).ok();
        let adapter = cfg.as_ref().and_then(|_| adapter_for(&r));

        // IDENTITY
        let prov = db
            .query_one("SELECT id FROM providers WHERE id=?", &[Val::from(r.text_or("provider_id", ""))])
            .await?;
        let id_problem: Option<String> = if cfg.is_none() {
            Some("config unparsable".to_string())
        } else if adapter.is_none() {
            Some(format!("no adapter for kind={} (unknown provenance)", r.text_or("kind", "")))
        } else if r.text_or("evidence_source", "").is_empty() {
            Some("no registration provenance".to_string())
        } else if prov.is_none() {
            Some("provider row missing".to_string())
        } else {
            None
        };
        if let Some(prob) = id_problem {
            rejected.push(json!({ "resource": rid, "gate": "IDENTITY",
                "reason": format!("unknown identity: {}", prob),
                "taxonomy": "CAPABILITY_FAILURE", "retryable": false }));
            continue;
        }
        let cfg = cfg.unwrap();
        let adapter = adapter.unwrap();
        // AUTHORITY (resource-level)
        if r.text_or("credential_state", "") == "invalid" {
            rejected.push(json!({ "resource": rid, "gate": "AUTHORITY",
                "reason": "credential invalid — resource disabled",
                "taxonomy": "AUTH_FAILURE", "retryable": false }));
            continue;
        }
        // DATA — policy evaluation per candidate.
        let mut blocked: Option<String> = None;
        for p in &policies {
            let rule: Value = serde_json::from_str(&p.text_or("rule", "{}")).unwrap_or(json!({}));
            let dw = rule.get("deny_when").cloned().unwrap_or(json!({}));
            let pid = p.text_or("id", "");
            let pname = p.text_or("name", "");
            if let (Some(rt), Some(dc_in)) = (
                dw.get("resource_trust").and_then(|v| v.as_str()),
                dw.get("data_class_in").and_then(|v| v.as_array()),
            ) {
                let cfg_trust = cfg.get("trust").and_then(|v| v.as_str()).unwrap_or("");
                let spec_dc = spec.get("data_class").and_then(|v| v.as_str()).unwrap_or("public");
                if cfg_trust == rt && dc_in.iter().any(|v| v.as_str() == Some(spec_dc)) {
                    blocked = Some(format!("policy {}: {}", pid, pname));
                }
            }
            if dw.get("cost_gt").and_then(|v| v.as_str()) == Some("job.max_cost") {
                let mc = spec.get("max_cost").and_then(|v| v.as_f64()).unwrap_or(0.0);
                if cost_of(&r) > mc {
                    blocked = Some(format!("policy {}: cost {} > max_cost {}", pid, cost_of(&r), mc));
                }
            }
        }
        if let Some(b) = blocked {
            rejected.push(json!({ "resource": rid, "gate": "DATA", "reason": b,
                "taxonomy": "POLICY_FAILURE", "retryable": false }));
            continue;
        }
        // QUOTA
        let q = db
            .query_one("SELECT * FROM quotas WHERE resource_id=?", &[Val::from(rid.clone())])
            .await?;
        let now_iso_s = now_iso();
        let (mut quota_reject, mut quota_retryable) = (None, false);
        if r.text_or("state", "") == "THROTTLED"
            && !r.text_or("throttled_until", "").is_empty()
            && r.text_or("throttled_until", "") > now_iso_s
        {
            quota_reject = Some(format!("throttled until {}", r.text_or("throttled_until", "")));
            quota_retryable = true;
        } else if r.text_or("state", "") == "EXHAUSTED"
            && !r.text_or("suspended_until", "").is_empty()
            && r.text_or("suspended_until", "") > now_iso_s
        {
            quota_reject = Some(format!("exhausted — suspended until recheck at {}", r.text_or("suspended_until", "")));
            quota_retryable = true;
        } else if let Some(ref qq) = q {
            let limit = qq.i64_or("limit_per_min", 0);
            if limit > 0 && qq.i64_or("used_window", 0) >= limit {
                quota_reject = Some(format!(
                    "quota exhausted ({}/{} per min)",
                    qq.i64_or("used_window", 0),
                    limit
                ));
                quota_retryable = true;
            }
        } else {
            let rl = db
                .query_one(
                    "SELECT COUNT(*) c FROM health_observations WHERE resource_id=? AND detail LIKE '%RATE_LIMITED%' AND observed_at>?",
                    &[Val::from(rid.clone()), Val::from(minutes_ago(60))],
                )
                .await?;
            if rl.map(|r| r.i64_or("c", 0)).unwrap_or(0) > 0 {
                quota_reject = Some("quota unknown and recent rate-limit history exists".to_string());
                quota_retryable = true;
            }
        }
        // (JS checks `!q || !q.limit_per_min` — the else branch above covers
        // q.is_none(); q with limit 0 also falls here via the limit>0 guard.
        // Match JS exactly: only when q missing or limit falsy.)
        let q_missing_or_zero = q.as_ref().map(|qq| qq.i64_or("limit_per_min", 0) <= 0).unwrap_or(true);
        if quota_reject.is_none() && q_missing_or_zero {
            let rl = db
                .query_one(
                    "SELECT COUNT(*) c FROM health_observations WHERE resource_id=? AND detail LIKE '%RATE_LIMITED%' AND observed_at>?",
                    &[Val::from(rid.clone()), Val::from(minutes_ago(60))],
                )
                .await?;
            if rl.map(|r| r.i64_or("c", 0)).unwrap_or(0) > 0 {
                quota_reject = Some("quota unknown and recent rate-limit history exists".to_string());
                quota_retryable = true;
            }
        }
        if let Some(qr) = quota_reject {
            rejected.push(json!({ "resource": rid, "gate": "QUOTA", "reason": qr,
                "taxonomy": "QUOTA_FAILURE", "retryable": quota_retryable }));
            continue;
        }
        // COST
        let effective_cost = cost_of(&r);
        let spend_auth = spec.get("spend_authority").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let max_cost = spec.get("max_cost").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if max_cost > spend_auth {
            rejected.push(json!({ "resource": rid, "gate": "COST",
                "reason": format!("declared max_cost ${} exceeds spend_authority ${}", max_cost, spend_auth),
                "taxonomy": "POLICY_FAILURE", "retryable": false }));
            continue;
        }
        if effective_cost > 0.0 && !(spend_auth > 0.0 && effective_cost <= spend_auth && effective_cost <= max_cost) {
            rejected.push(json!({ "resource": rid, "gate": "COST",
                "reason": format!("effective cost ${} requires explicit spend_authority (authorized ${}, max_cost ${})", effective_cost, spend_auth, max_cost),
                "taxonomy": "POLICY_FAILURE", "retryable": false }));
            continue;
        }
        if effective_cost > max_cost {
            rejected.push(json!({ "resource": rid, "gate": "COST",
                "reason": format!("cost ${} exceeds authorized max ${}", effective_cost, max_cost),
                "taxonomy": "POLICY_FAILURE", "retryable": false }));
            continue;
        }
        // INTEGRITY — re-read fresh state.
        let fresh = db
            .query_one("SELECT state,evidence_state FROM resources WHERE id=?", &[Val::from(rid.clone())])
            .await?
            .unwrap_or_default();
        let mut int_reject: Option<String> = None;
        if fresh.text_or("state", "") == "QUARANTINED" {
            int_reject = Some(format!("state {}", fresh.text_or("state", "")));
        } else if !EVIDENCE_USABLE.contains(&fresh.text_or("evidence_state", "").as_str()) {
            int_reject = Some(format!("evidence {} (changed since shortlist)", fresh.text_or("evidence_state", "")));
        } else {
            let last = db
                .query_one(
                    "SELECT observed_at FROM health_observations WHERE resource_id=? ORDER BY observed_at DESC LIMIT 1",
                    &[Val::from(rid.clone())],
                )
                .await?;
            let age_min = last
                .map(|r| (chrono::Utc::now().timestamp_millis() - ts(&r.text_or("observed_at", ""))) as f64 / 60000.0)
                .unwrap_or(9999.0);
            if age_min > stale_after_ev(&fresh.text_or("evidence_state", "")) as f64 {
                if adapter.claimed() {
                    // Claimed: skip the probe (it would always fail); the
                    // cron's demoteCheck handles staleness.
                } else {
                    let p = adapter.probe(&r, env).await;
                    observe(db, &rid, p.ok, p.latency_ms, &format!("integrity revalidation: {}", p.detail)).await?;
                    if !p.ok {
                        int_reject = Some(format!("stale ({}m) and revalidation failed: {}", age_min.round() as i64, p.detail));
                    }
                }
            }
        }
        if let Some(ir) = int_reject {
            rejected.push(json!({ "resource": rid, "gate": "INTEGRITY", "reason": ir,
                "taxonomy": "RETRYABLE_FAILURE", "retryable": true }));
            continue;
        }
        survivors.push((
            r,
            Survivor { id: rid, cfg, quota: q, effective_cost },
        ));
        let _ = adapter;
    }

    for gn in ["IDENTITY", "AUTHORITY", "DATA", "EVIDENCE", "QUOTA", "COST", "INTEGRITY"] {
        let fails: Vec<&Value> = rejected.iter().filter(|x| x["gate"] == gn).collect();
        if !fails.is_empty() {
            gate(
                &mut trace,
                gn,
                false,
                &format!("{} candidate(s) rejected at {}", fails.len(), gn),
                json!({ "resources": fails.iter().map(|x| x["resource"].clone()).collect::<Vec<_>>() }),
            );
        }
        if !survivors.is_empty() {
            gate(
                &mut trace,
                gn,
                true,
                &format!("{} candidate(s) passed {}", survivors.len(), gn),
                json!({ "resources": survivors.iter().map(|s| Value::from(s.1.id.clone())).collect::<Vec<_>>() }),
            );
        }
    }
    if survivors.is_empty() {
        vacuous(&mut trace, &gate_order);
        return Ok(GovernResult { trace, selected: None, rejected, ranked: vec![] });
    }

    // EFFICIENCY — deterministic scoring.
    let mut scored: Vec<Value> = Vec::new();
    for (r, s) in &survivors {
        let h = db
            .query_one(
                "SELECT COUNT(*) c, SUM(ok) ok, AVG(latency_ms) lat FROM (SELECT * FROM health_observations WHERE resource_id=? ORDER BY observed_at DESC LIMIT 20)",
                &[Val::from(s.id.clone())],
            )
            .await?
            .unwrap_or_default();
        let probes = h.i64_or("c", 0);
        let ok_sum = h.i64_or("ok", 0);
        let reliability = if probes > 0 { ok_sum as f64 / probes as f64 } else { 0.5 };
        let avg_lat = h.f64("lat").unwrap_or(20000.0);
        let latency_score = (1.0 - avg_lat / 60000.0).max(0.0);
        let avail_score = match r.text_or("state", "").as_str() {
            "AVAILABLE" => 1.0,
            "DEGRADED" => 0.6,
            _ => 0.0,
        };
        let quota_score = match &s.quota {
            Some(qq) if qq.i64_or("limit_per_min", 0) > 0 => {
                1.0 - qq.i64_or("used_window", 0) as f64 / qq.i64_or("limit_per_min", 1) as f64
            }
            _ => 1.0,
        };
        let cost_score = match r.text_or("cost_class", "").as_str() {
            "free" => 1.0,
            "credit-backed" => 0.8,
            _ => 0.2,
        };
        let sov_score = match r.text_or("sovereignty", "").as_str() {
            "local" => 1.0,
            "self-hosted" => 0.8,
            _ => 0.5,
        };
        let expected_success = (reliability * 1000.0).round() / 1000.0;
        let human_cost: f64 = ((if s.cfg.get("trust").and_then(|v| v.as_str()) == Some("untrusted") { 0.2f64 } else { 0.0f64 }
            + if r.text_or("sovereignty", "") == "external" { 0.05f64 } else { 0.0f64 })
            * 100.0)
            .round()
            / 100.0;
        let economic = (expected_success * (1.0 - human_cost)
            * if s.effective_cost == 0.0 { 1.0 } else { (1.0 - s.effective_cost * 5.0).max(0.0)}
            * 10000.0)
            .round()
            / 10000.0;
        let score = ((0.20 * 1.0
            + 0.15 * reliability
            + 0.10 * latency_score
            + 0.10 * avail_score
            + 0.10 * quota_score
            + 0.10 * cost_score
            + 0.05 * sov_score
            + 0.20 * economic)
            * 10000.0)
            .round()
            / 10000.0;
        scored.push(json!({
            "resource": s.id, "score": score,
            "breakdown": {
                "capability_fit": 1.0,
                "reliability": (reliability * 1000.0).round() / 1000.0,
                "latency_ms": avg_lat.round() as i64,
                "latency_score": (latency_score * 1000.0).round() / 1000.0,
                "availability": avail_score,
                "quota_remaining": (quota_score * 1000.0).round() / 1000.0,
                "cost_class": r.text_or("cost_class", ""),
                "cost_score": cost_score,
                "sovereignty": r.text_or("sovereignty", ""),
                "sovereignty_score": sov_score,
                "health_probes": probes,
                "effective_cost": s.effective_cost,
                "expected_success": expected_success,
                "human_intervention_cost": human_cost,
                "economic_score": economic,
            },
        }));
    }
    scored.sort_by(|a, b| {
        b["score"].as_f64().unwrap_or(0.0).partial_cmp(&a["score"].as_f64().unwrap_or(0.0)).unwrap()
    });
    let winner = scored[0].clone();
    for sc in scored.iter().skip(1) {
        rejected.push(json!({ "resource": sc["resource"].clone(), "gate": "EFFICIENCY",
            "reason": format!("outscored by {} ({} < {})", winner["resource"].as_str().unwrap_or(""),
                sc["score"], winner["score"]),
            "taxonomy": Value::Null, "retryable": false }));
    }
    gate(
        &mut trace,
        "EFFICIENCY",
        true,
        &format!("selected {} (score {}), {} fallback(s)",
            winner["resource"].as_str().unwrap_or(""), winner["score"], scored.len() - 1),
        json!({ "candidates": scored }),
    );
    let ranked: Vec<Value> = scored
        .iter()
        .map(|sc| json!({ "resource_id": sc["resource"].clone(), "score": sc["score"].clone(), "breakdown": sc["breakdown"].clone() }))
        .collect();
    let selected = json!({ "resource_id": winner["resource"].clone(),
        "score": winner["score"].clone(), "breakdown": winner["breakdown"].clone() });
    Ok(GovernResult { trace, selected: Some(selected), rejected, ranked })
}

fn vacuous(trace: &mut Vec<Value>, gate_order: &[&str]) {
    let recorded: std::collections::HashSet<String> =
        trace.iter().filter_map(|x| x["gate"].as_str().map(|s| s.to_string())).collect();
    for gn in gate_order {
        if !recorded.contains(*gn) {
            gate(trace, gn, true, "vacuous — no candidate reached this gate", json!({}));
        }
    }
}

/// unclaimedQueueDepth: delegated leases awaiting pickup for a resource.
pub async fn unclaimed_queue_depth(db: &dyn Db, resource_id: &str, now_iso_s: &str) -> Result<i64, DbError> {
    let r = db
        .query_one(
            "SELECT COUNT(*) c FROM leases l JOIN jobs j ON j.id=l.job_id
             LEFT JOIN claims c ON c.lease_id=l.id
             WHERE l.resource_id=? AND l.state='active' AND l.expires_at > ?
               AND j.state='deferred' AND c.claim_id IS NULL",
            &[Val::from(resource_id.to_string()), Val::from(now_iso_s.to_string())],
        )
        .await?;
    Ok(r.map(|r| r.i64_or("c", 0)).unwrap_or(0))
}

/// submitJob: validate → dedupe → insert → runJob.
pub async fn submit_job(db: &dyn Db, spec: &Value, env: &Env) -> Result<Value, DbError> {
    let errs = validate_job(spec);
    if !errs.is_empty() {
        return Ok(json!({ "error": errs.join("; "), "status": 400 }));
    }
    if let Some(ik) = spec.get("idempotency_key").and_then(|v| v.as_str()) {
        let dup = db
            .query_one("SELECT id,state FROM jobs WHERE id=?", &[Val::from(format!("job_{}", ik))])
            .await?;
        if let Some(d) = dup {
            return Ok(json!({ "job_id": d.text_or("id", ""), "state": d.text_or("state", ""),
                              "deduplicated": true, "status": 200 }));
        }
    }
    let job_id = spec
        .get("idempotency_key")
        .and_then(|v| v.as_str())
        .map(|ik| format!("job_{}", ik))
        .unwrap_or_else(|| uid("job"));
    let input_hash = sha256hex(&serde_json::to_string(&spec["input"]).unwrap_or_default());
    db.exec(
        "INSERT INTO jobs (id,organelle,capability,input_hash,spec,data_class,max_cost,priority,state,defer_count) VALUES (?,?,?,?,?,?,?,?,?,?)",
        &[
            Val::from(job_id.clone()),
            Val::from(spec.get("organelle").and_then(|v| v.as_str()).unwrap_or("unknown").to_string()),
            Val::from(spec.get("capability").and_then(|v| v.as_str()).unwrap_or("").to_string()),
            Val::from(input_hash),
            Val::from(spec.to_string()),
            Val::from(spec.get("data_class").and_then(|v| v.as_str()).unwrap_or("public").to_string()),
            Val::from(spec.get("max_cost").and_then(|v| v.as_f64()).unwrap_or(0.0)),
            Val::from(spec.get("priority").and_then(|v| v.as_i64()).unwrap_or(5)),
            Val::from("submitted"),
            Val::from(0i64),
        ],
    )
    .await?;
    run_job(db, &job_id, spec, env, false).await
}

/// quotaConsume: consume one unit of quota; exhaust → suspend.
pub async fn quota_consume(db: &dyn Db, resource_id: &str) -> Result<(), DbError> {
    let q = db
        .query_one("SELECT * FROM quotas WHERE resource_id=?", &[Val::from(resource_id.to_string())])
        .await?;
    let q = match q {
        Some(q) => q,
        None => return Ok(()),
    };
    if q.i64_or("limit_per_min", 0) <= 0 {
        return Ok(());
    }
    let prev = db
        .query_one("SELECT state FROM resources WHERE id=?", &[Val::from(resource_id.to_string())])
        .await
        .map(|o| o.map(|r| r.text_or("state", "UNKNOWN")).unwrap_or("UNKNOWN".to_string()))
        .unwrap_or("UNKNOWN".to_string());
    let now_ms = chrono::Utc::now().timestamp_millis();
    let window_start = if q.text_or("window_started_at", "").is_empty() {
        0
    } else {
        ts(&q.text_or("window_started_at", ""))
    };
    let used: i64;
    if now_ms - window_start > 60000 {
        used = 1;
        let iso = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        let space = now_iso().replace('T', " ").chars().take(19).collect::<String>();
        db.exec(
            "UPDATE quotas SET used_window=1, window_started_at=?, last_checked_at=? WHERE resource_id=?",
            &[Val::from(iso), Val::from(space), Val::from(resource_id.to_string())],
        )
        .await?;
    } else {
        used = q.i64_or("used_window", 0) + 1;
        let space = now_iso().replace('T', " ").chars().take(19).collect::<String>();
        db.exec(
            "UPDATE quotas SET used_window=used_window+1, last_checked_at=? WHERE resource_id=?",
            &[Val::from(space), Val::from(resource_id.to_string())],
        )
        .await?;
    }
    if used >= q.i64_or("limit_per_min", 0) && prev != "EXHAUSTED" {
        let until = (chrono::Utc::now() + chrono::Duration::minutes(5))
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        db.exec(
            "UPDATE resources SET state='EXHAUSTED', suspended_until=? WHERE id=?",
            &[Val::from(until.clone()), Val::from(resource_id.to_string())],
        )
        .await?;
        db.exec(
            "INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)",
            &[
                Val::from(resource_id.to_string()),
                Val::from(format!("health:{}", prev)),
                Val::from("health:EXHAUSTED"),
                Val::from("quota-exhausted"),
                Val::from(format!("window {}/{} consumed — suspended until recheck at {}",
                    used, q.i64_or("limit_per_min", 0), until)),
            ],
        )
        .await?;
    }
    Ok(())
}

/// runJob: govern → lease → revalidate → execute attempts → verify → receipt.
pub async fn run_job(
    db: &dyn Db,
    job_id: &str,
    spec: &Value,
    env: &Env,
    skip_backpressure: bool,
) -> Result<Value, DbError> {
    let policy_v = policy_version(db).await?;
    let g = govern(db, spec, env).await?;
    // BACKPRESSURE
    if !skip_backpressure {
        if let Some(ref sel) = g.selected {
            let rid = sel["resource_id"].as_str().unwrap_or("");
            let depth = unclaimed_queue_depth(db, rid, &now_iso()).await?;
            if depth >= QUEUE_FULL_THRESHOLD {
                db.exec("DELETE FROM jobs WHERE id=?", &[Val::from(job_id.to_string())]).await?;
                return Ok(json!({ "error": "queue-full", "resource": rid,
                    "queue_depth": depth, "threshold": QUEUE_FULL_THRESHOLD,
                    "retry_after_s": 60, "status": 429 }));
            }
        }
    }
    let mut trace = g.trace;
    let input_hash = db
        .query_one("SELECT input_hash FROM jobs WHERE id=?", &[Val::from(job_id.to_string())])
        .await
        .map(|o| o.map(|r| r.text_or("input_hash", "")).unwrap_or_default())
        .unwrap_or_default();

    if g.selected.is_none() {
        for gn in ["EFFICIENCY", "LEASE"] {
            if !trace.iter().any(|x| x["gate"] == gn) {
                gate(&mut trace, gn, true, "vacuous — no execution attempted", Value::Null);
            }
        }
        let any_retryable = !g.rejected.is_empty()
            && g.rejected.iter().any(|x| x["retryable"].as_bool().unwrap_or(false));
        let result = if any_retryable { "deferred" } else { "blocked" };
        let receipt_id = uid("rcpt");
        let errors = g
            .rejected
            .iter()
            .map(|x| {
                format!(
                    "{} [{}/{}]: {}",
                    x["resource"].as_str().unwrap_or(""),
                    x["gate"].as_str().unwrap_or(""),
                    x["taxonomy"].as_str().unwrap_or("n/a"),
                    x["reason"].as_str().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join(" | ");
        let errors = if errors.is_empty() { "no valid resource".to_string() } else { errors };
        db.exec(
            "INSERT INTO receipts (id,job_id,provider,resource,input_hash,result,cost,policy_version,decision,fallback_chain,errors) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
            &[
                Val::from(receipt_id.clone()),
                Val::from(job_id.to_string()),
                Val::Null, Val::Null,
                Val::from(input_hash),
                Val::from(result),
                Val::from(0i64),
                Val::from(policy_v.clone()),
                Val::from(json!({ "trace": trace, "rejected": g.rejected, "selected": Value::Null }).to_string()),
                Val::from(json!([]).to_string()),
                Val::from(errors),
            ],
        )
        .await?;
        db.exec("UPDATE jobs SET state=? WHERE id=?", &[Val::from(result), Val::from(job_id.to_string())]).await?;
        return Ok(json!({ "job_id": job_id, "state": result, "governor_trace": trace,
                          "rejected": g.rejected, "receipt_id": receipt_id, "status": 200 }));
    }

    db.exec("UPDATE jobs SET state='running' WHERE id=?", &[Val::from(job_id.to_string())]).await?;
    let execution_id = uid("exec");
    let started = now_iso();
    let mut fallback_chain: Vec<Value> = Vec::new();
    let mut attempt_no = 0i64;
    let mut final_ok: Option<Value> = None;
    let mut final_lease_snap = Value::Null;
    let mut final_attempt_id = String::new();
    let (mut health_before, mut health_after) = (String::new(), String::new());
    let (mut quota_before, mut quota_after) = (0i64, 0i64);
    let (mut cost_before, mut cost_after) = (0.0, 0.0);

    for cand in &g.ranked {
        attempt_no += 1;
        let attempt_id = format!("{}_a{}", execution_id, attempt_no);
        let cand_rid = cand["resource_id"].as_str().unwrap_or("");
        let mut resource = db
            .query_one("SELECT * FROM resources WHERE id=?", &[Val::from(cand_rid.to_string())])
            .await?
            .unwrap_or_default();
        let provider = db
            .query_one(
                "SELECT * FROM providers WHERE id=?",
                &[Val::from(resource.text_or("provider_id", ""))],
            )
            .await?
            .unwrap_or_default();
        let adapter = match adapter_for(&resource) {
            Some(a) => a,
            None => continue,
        };
        let q0 = db
            .query_one(
                "SELECT used_window,limit_per_min FROM quotas WHERE resource_id=?",
                &[Val::from(resource.text_or("id", ""))],
            )
            .await?;
        health_before = resource.text_or("state", "");
        quota_before = q0.as_ref().map(|r| r.i64_or("used_window", 0)).unwrap_or(0);
        cost_before = cost_of(&resource);

        // DELEGATED resource: delegateExecution owns the lease.
        if adapter.claimed() {
            let d = delegate_execution(db, job_id, spec, &resource, &provider, &mut trace).await?;
            return Ok(json!({ "job_id": job_id, "state": "deferred",
                "lease_id": d["lease_id"].clone(), "resource": resource.text_or("id", ""),
                "receipt_id": d["receipt_id"].clone(), "governor_trace": d["trace"].clone(),
                "status": 200 }));
        }

        // LEASE
        let lease = create_lease(
            db,
            job_id,
            &resource.text_or("id", ""),
            cost_of(&resource),
            &json!({ "limit_per_min": q0.as_ref().and_then(|r| r.i64("limit_per_min")),
                     "used_window": quota_before }),
            &policy_v,
        )
        .await?;
        let lease_id = lease["lease_id"].clone();
        trace.push(json!({ "gate": "LEASE", "pass": true,
            "reason": format!("lease {} active until {}", lease_id, lease["expires_at"]),
            "detail": { "resource": resource.text_or("id", ""), "expected_cost": cost_of(&resource),
                        "policy_version": policy_v } }));
        db.exec(
            "INSERT INTO executions (id,job_id,resource_id,started_at,result,attempt_no,lease_id,request_hash) VALUES (?,?,?,?,?,?,?,?)",
            &[
                Val::from(attempt_id.clone()),
                Val::from(job_id.to_string()),
                Val::from(resource.text_or("id", "")),
                Val::from(now_iso()),
                Val::from("running"),
                Val::from(attempt_no),
                Val::from(lease_id.clone()),
                Val::from(input_hash.clone()),
            ],
        )
        .await?;

        // Revalidate before dispatch.
        let lv = validate_lease(db, &lease_id).await?;
        resource = db
            .query_one("SELECT * FROM resources WHERE id=?", &[Val::from(cand_rid.to_string())])
            .await?
            .unwrap_or_default();
        let q1 = db
            .query_one(
                "SELECT used_window,limit_per_min FROM quotas WHERE resource_id=?",
                &[Val::from(resource.text_or("id", ""))],
            )
            .await?;
        let invalid: Option<String> = if !lv.0 {
            Some(format!("lease {}", lv.1))
        } else if !EVIDENCE_USABLE.contains(&resource.text_or("evidence_state", "UNKNOWN").as_str()) {
            Some(format!("evidence changed to {}", resource.text_or("evidence_state", "")))
        } else if ["QUARANTINED", "EXHAUSTED"].contains(&resource.text_or("state", "").as_str()) {
            Some(format!("state changed to {}", resource.text_or("state", "")))
        } else if resource.text_or("state", "") == "THROTTLED"
            && resource.text_or("throttled_until", "") > now_iso()
        {
            Some("throttled since lease issued".to_string())
        } else if cost_of(&resource) != cost_before {
            Some(format!("cost changed ${} → ${} since lease", cost_before, cost_of(&resource)))
        } else if q1.as_ref().map(|r| r.i64_or("limit_per_min", 0)).unwrap_or(0) > 0
            && q1.as_ref().map(|r| r.i64_or("used_window", 0)).unwrap_or(0)
                >= q1.as_ref().map(|r| r.i64_or("limit_per_min", 0)).unwrap_or(0)
        {
            Some("quota exhausted since lease issued".to_string())
        } else {
            None
        };

        if let Some(inv) = invalid {
            db.exec("UPDATE leases SET state='invalidated' WHERE id=?", &[Val::from(lease_id.clone())]).await?;
            db.exec(
                "UPDATE executions SET result=?, error=?, error_class=?, failure_taxonomy=?, finished_at=? WHERE id=?",
                &[Val::from("error"),
                  Val::from(format!("lease invalidated — not executed: {}", inv)),
                  Val::Null, Val::from("RETRYABLE_FAILURE"), Val::from(now_iso()),
                  Val::from(attempt_id.clone())],
            )
            .await?;
            fallback_chain.push(json!({ "attempt": attempt_id, "resource": resource.text_or("id", ""),
                "lease": lease_id, "executed": false, "taxonomy": "RETRYABLE_FAILURE",
                "error": format!("lease invalidated — not executed: {}", inv) }));
            trace.push(json!({ "gate": "EXECUTE", "pass": false,
                "reason": format!("not executed — {}", inv),
                "detail": { "resource": resource.text_or("id", ""), "attempt": attempt_id } }));
            continue;
        }

        // EXECUTE
        let r = adapter.execute(&resource, spec, env).await;
        let taxonomy = taxonomy_of(r.error_class.as_deref().unwrap_or("UNKNOWN")).to_string();
        observe(
            db,
            &resource.text_or("id", ""),
            r.ok,
            r.latency_ms,
            &if r.ok { "job execution".to_string() } else {
                format!("{}: {}", r.error_class.clone().unwrap_or_else(|| "UNKNOWN".to_string()),
                    r.error.clone().unwrap_or_default())
            },
        )
        .await?;
        quota_consume(db, &resource.text_or("id", "")).await?;
        db.exec(
            "UPDATE leases SET state=? WHERE id=?",
            &[Val::from(if r.ok { "consumed" } else { "released" }), Val::from(lease_id.clone())],
        )
        .await?;
        let q2 = db
            .query_one(
                "SELECT used_window FROM quotas WHERE resource_id=?",
                &[Val::from(resource.text_or("id", ""))],
            )
            .await?;
        let rs2 = db
            .query_one(
                "SELECT state FROM resources WHERE id=?",
                &[Val::from(resource.text_or("id", ""))],
            )
            .await?
            .unwrap_or_default();
        quota_after = q2.map(|r| r.i64_or("used_window", 0)).unwrap_or(0);
        health_after = rs2.text_or("state", "");
        cost_after = cost_of(&resource);

        if r.ok {
            let content = r.output.as_ref().and_then(|o| o.get("content"));
            let verified = content.is_none()
                || matches!(content, Some(Value::String(s)) if !s.is_empty());
            if !verified {
                db.exec(
                    "UPDATE executions SET result=?, error=?, error_class=?, failure_taxonomy=?, finished_at=? WHERE id=?",
                    &[Val::from("error"),
                      Val::from("adapter returned ok with empty content"),
                      Val::from("VERIFICATION_FAILED"),
                      Val::from("VERIFICATION_FAILURE"),
                      Val::from(now_iso()),
                      Val::from(attempt_id.clone())],
                )
                .await?;
                fallback_chain.push(json!({ "attempt": attempt_id, "resource": resource.text_or("id", ""),
                    "lease": lease_id, "executed": true, "taxonomy": "VERIFICATION_FAILURE",
                    "error_class": "VERIFICATION_FAILED", "error": "adapter returned ok with empty content" }));
                trace.push(json!({ "gate": "EXECUTE", "pass": false,
                    "reason": "VERIFICATION_FAILURE: empty content",
                    "detail": { "resource": resource.text_or("id", ""), "attempt": attempt_id } }));
                continue;
            }
            let output_hash = sha256hex(&serde_json::to_string(r.output.as_ref().unwrap_or(&Value::Null)).unwrap_or_default());
            db.exec(
                "UPDATE executions SET result='ok', output_hash=?, finished_at=? WHERE id=?",
                &[Val::from(output_hash.clone()), Val::from(now_iso()), Val::from(attempt_id.clone())],
            )
            .await?;
            trace.push(json!({ "gate": "EXECUTE", "pass": true,
                "reason": format!("executed on {} (verified)", resource.text_or("id", "")),
                "detail": { "resource": resource.text_or("id", ""), "attempt": attempt_id,
                            "latency_ms": r.latency_ms } }));
            final_ok = Some(json!({ "resource": resource.text_or("id", ""),
                "provider": provider.text_or("id", ""),
                "output": r.output.clone().unwrap_or(Value::Null),
                "output_hash": output_hash }));
            final_lease_snap = db
                .query_one("SELECT * FROM leases WHERE id=?", &[Val::from(lease_id.clone())])
                .await?
                .map(|r| crate::db::row_to_json(&r))
                .unwrap_or(Value::Null);
            final_attempt_id = attempt_id;
            break;
        }
        db.exec(
            "UPDATE executions SET result=?, error=?, error_class=?, failure_taxonomy=?, finished_at=? WHERE id=?",
            &[Val::from("error"),
              r.error.clone().map(Val::from).unwrap_or(Val::Null),
              r.error_class.clone().map(Val::from).unwrap_or(Val::from("UNKNOWN")),
              Val::from(taxonomy.clone()),
              Val::from(now_iso()),
              Val::from(attempt_id.clone())],
        )
        .await?;
        fallback_chain.push(json!({ "attempt": attempt_id, "resource": resource.text_or("id", ""),
            "lease": lease_id, "executed": true, "taxonomy": taxonomy,
            "error_class": r.error_class.clone().unwrap_or_else(|| "UNKNOWN".to_string()),
            "error": r.error.clone().unwrap_or_default() }));
        trace.push(json!({ "gate": "EXECUTE", "pass": false,
            "reason": format!("{}: {}", taxonomy, r.error.unwrap_or_default()),
            "detail": { "resource": resource.text_or("id", ""), "attempt": attempt_id } }));
    }

    let finished = now_iso();
    if let Some(fok) = final_ok {
        let receipt_id = uid("rcpt");
        let errors: Option<String> = if fallback_chain.is_empty() {
            None
        } else {
            Some(
                fallback_chain
                    .iter()
                    .map(|f| {
                        format!(
                            "{} [{}]: {}",
                            f["resource"].as_str().unwrap_or(""),
                            f["taxonomy"].as_str().unwrap_or(""),
                            f["error"].as_str().unwrap_or("")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" | "),
            )
        };
        db.exec(
            "INSERT INTO receipts (id,job_id,execution_id,attempt_id,provider,resource,started_at,finished_at,input_hash,output_hash,result,cost,cost_before,cost_after,quota_before,quota_after,health_before,health_after,policy_version,lease,decision,fallback_chain,errors) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            &[
                Val::from(receipt_id.clone()),
                Val::from(job_id.to_string()),
                Val::from(execution_id.clone()),
                Val::from(final_attempt_id.clone()),
                Val::from(fok["provider"].as_str().unwrap_or("").to_string()),
                Val::from(fok["resource"].as_str().unwrap_or("").to_string()),
                Val::from(started),
                Val::from(finished),
                Val::from(input_hash),
                Val::from(fok["output_hash"].as_str().unwrap_or("").to_string()),
                Val::from("ok"),
                Val::from(cost_after),
                Val::from(cost_before),
                Val::from(cost_after),
                Val::from(quota_before.to_string()),
                Val::from(quota_after.to_string()),
                Val::from(health_before),
                Val::from(health_after),
                Val::from(policy_v.clone()),
                Val::from(final_lease_snap.to_string()),
                Val::from(json!({ "trace": trace, "rejected": g.rejected,
                    "selected": g.selected, "output": fok["output"].clone() }).to_string()),
                Val::from(serde_json::to_string(&fallback_chain).unwrap_or_default()),
                errors.map(Val::from).unwrap_or(Val::Null),
            ],
        )
        .await?;
        db.exec(
            "INSERT INTO accounting (id,execution_id,claim_id,job_id,resource_id,measured_cost,cost_basis,quota_effect) VALUES (?,?,?,?,?,?,?,?)",
            &[
                Val::from(uid("acct")),
                Val::from(execution_id.clone()),
                Val::Null,
                Val::from(job_id.to_string()),
                Val::from(fok["resource"].as_str().unwrap_or("").to_string()),
                Val::from(cost_after),
                Val::from("unknown"),
                Val::from(json!({ "quota_before": quota_before, "quota_after": quota_after }).to_string()),
            ],
        )
        .await?;
        db.exec("UPDATE jobs SET state='done' WHERE id=?", &[Val::from(job_id.to_string())]).await?;
        return Ok(json!({ "job_id": job_id, "state": "done", "receipt_id": receipt_id,
            "execution_id": execution_id, "resource": fok["resource"].clone(),
            "output": fok["output"].clone(), "governor_trace": trace, "status": 200 }));
    }

    // Every attempt failed.
    let taxonomies: Vec<String> = fallback_chain
        .iter()
        .filter_map(|f| f["taxonomy"].as_str().map(|s| s.to_string()))
        .collect();
    let all_retryable = !taxonomies.is_empty()
        && taxonomies.iter().any(|t| t == "RETRYABLE_FAILURE" || t == "QUOTA_FAILURE");
    let defer_count = db
        .query_one("SELECT defer_count FROM jobs WHERE id=?", &[Val::from(job_id.to_string())])
        .await
        .map(|o| o.map(|r| r.i64_or("defer_count", 0)).unwrap_or(0))
        .unwrap_or(0);
    let result = if all_retryable && defer_count < 8 { "deferred" } else { "failed" };
    let receipt_id = uid("rcpt");
    let err_text = fallback_chain
        .iter()
        .map(|f| {
            format!(
                "{} [{}]: {}",
                f["resource"].as_str().unwrap_or(""),
                f["taxonomy"].as_str().unwrap_or(""),
                f["error"].as_str().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join(" | ");
    let err_text = if err_text.is_empty() { "no candidate executed".to_string() } else { err_text };
    db.exec(
        "INSERT INTO receipts (id,job_id,execution_id,provider,resource,started_at,finished_at,input_hash,result,cost,policy_version,decision,fallback_chain,errors) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        &[
            Val::from(receipt_id.clone()),
            Val::from(job_id.to_string()),
            Val::from(execution_id.clone()),
            Val::Null, Val::Null,
            Val::from(started),
            Val::from(finished),
            Val::from(input_hash),
            Val::from(result),
            Val::from(0i64),
            Val::from(policy_v),
            Val::from(json!({ "trace": trace, "rejected": g.rejected, "selected": g.selected }).to_string()),
            Val::from(serde_json::to_string(&fallback_chain).unwrap_or_default()),
            Val::from(err_text.clone()),
        ],
    )
    .await?;
    db.exec("UPDATE jobs SET state=? WHERE id=?", &[Val::from(result), Val::from(job_id.to_string())]).await?;
    Ok(json!({ "job_id": job_id, "state": result, "receipt_id": receipt_id,
        "execution_id": execution_id, "error": err_text, "governor_trace": trace, "status": 200 }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_job_rejects() {
        assert!(!validate_job(&json!({})).is_empty());
        assert!(!validate_job(&json!({"capability": "x"})).is_empty()); // missing input
        assert!(!validate_job(&json!({"capability": "x", "input": {}, "data_class": "bogus"})).is_empty());
        assert!(!validate_job(&json!({"capability": "x", "input": {}, "max_cost": -1})).is_empty());
    }

    #[test]
    fn validate_job_accepts() {
        assert!(validate_job(&json!({"capability": "inference", "input": {"a": 1}})).is_empty());
        assert!(validate_job(&json!({"capability": "inference", "input": {}, "data_class": "secret", "max_cost": 5, "spend_authority": 5})).is_empty());
    }
}
