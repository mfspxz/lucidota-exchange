//! Observation + evidence ladder — port of worker.js.
//!
//! `observe()` is the anti-poisoning health rollup (3-strike quarantine
//! requires a governor-observed failure; worker 429s/AUTH_ERRORs need
//! corroboration before they drive disruptive transitions). The evidence
//! ladder (DISCOVERED → … → VERIFIED) is rank-guarded: demotions need
//! `force=true`.

use crate::db::{Db, DbError, RowExt, Val};
use crate::env::Env;
use crate::util::{minutes_ago, now_iso, ts};
use serde_json::{json, Value};

pub const EVIDENCE_LADDER: &[&str] = &[
    "DISCOVERED",
    "ACCESSIBLE",
    "FREE",
    "QUOTA",
    "AVAILABLE",
    "VERIFIED",
];
pub const EVIDENCE_USABLE: &[&str] = &["AVAILABLE", "VERIFIED"];

pub fn ev_rank(s: &str) -> i64 {
    if s == "PARKED" {
        return -2;
    }
    if s == "UNKNOWN" {
        return -1;
    }
    EVIDENCE_LADDER.iter().position(|x| *x == s).map(|i| i as i64).unwrap_or(-1)
}

/// setEvidence with the ladder guard: demotions require force=true.
pub async fn set_evidence(
    db: &dyn Db,
    rid: &str,
    to: &str,
    trigger: &str,
    detail: &str,
    force: bool,
) -> Result<Value, DbError> {
    let cur = db
        .query_one("SELECT evidence_state FROM resources WHERE id=?", &[Val::from(rid.to_string())])
        .await?;
    let from = cur.and_then(|r| r.text("evidence_state")).unwrap_or_else(|| "UNKNOWN".to_string());
    if from == to {
        return Ok(json!({ "from": from, "to": to, "changed": false }));
    }
    if !force && ev_rank(to) < ev_rank(&from) {
        return Ok(json!({ "from": from, "to": to, "changed": false, "skipped": "demotion requires force" }));
    }
    db.exec(
        "UPDATE resources SET evidence_state=?, evidence_at=? WHERE id=?",
        &[Val::from(to.to_string()), Val::from(now_iso()), Val::from(rid.to_string())],
    )
    .await?;
    db.exec(
        "INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)",
        &[
            Val::from(rid.to_string()),
            Val::from(from.clone()),
            Val::from(to.to_string()),
            Val::from(trigger.to_string()),
            Val::from(detail.to_string()),
        ],
    )
    .await?;
    Ok(json!({ "from": from, "to": to, "changed": true }))
}

/// Tier info: the tier warranted by recorded evidence plus the counts.
pub struct TierInfo {
    pub tier: String,
    pub ok7: i64,
    pub ok24: i64,
    pub exec_ok: i64,
    pub free_exec: i64,
    pub quota_measured: bool,
}

pub async fn warranted_tier_info(db: &dyn Db, rid: &str) -> Result<TierInfo, DbError> {
    let r = db
        .query_one("SELECT state FROM resources WHERE id=?", &[Val::from(rid.to_string())])
        .await?;
    let state = match r {
        None => {
            return Ok(TierInfo {
                tier: "UNKNOWN".to_string(),
                ok7: 0, ok24: 0, exec_ok: 0, free_exec: 0,
                quota_measured: false,
            })
        }
        Some(r) => r.text_or("state", ""),
    };
    let ok7 = db
        .query_one(
            "SELECT COUNT(*) c FROM health_observations WHERE resource_id=? AND ok=1 AND observed_at>?",
            &[Val::from(rid.to_string()), Val::from(minutes_ago(7 * 24 * 60))],
        )
        .await
        .map(|o| o.map(|r| r.i64_or("c", 0)).unwrap_or(0))
        .unwrap_or(0);
    let ok24 = db
        .query_one(
            "SELECT COUNT(*) c FROM health_observations WHERE resource_id=? AND ok=1 AND observed_at>?",
            &[Val::from(rid.to_string()), Val::from(minutes_ago(24 * 60))],
        )
        .await
        .map(|o| o.map(|r| r.i64_or("c", 0)).unwrap_or(0))
        .unwrap_or(0);
    let exec_ok = db
        .query_one(
            "SELECT COUNT(*) c FROM executions WHERE resource_id=? AND result='ok'",
            &[Val::from(rid.to_string())],
        )
        .await
        .map(|o| o.map(|r| r.i64_or("c", 0)).unwrap_or(0))
        .unwrap_or(0);
    let free_exec = db
        .query_one(
            "SELECT COUNT(*) c FROM receipts WHERE resource=? AND result='ok' AND (cost IS NULL OR cost=0)",
            &[Val::from(rid.to_string())],
        )
        .await
        .map(|o| o.map(|r| r.i64_or("c", 0)).unwrap_or(0))
        .unwrap_or(0);
    let q = db
        .query_one("SELECT * FROM quotas WHERE resource_id=?", &[Val::from(rid.to_string())])
        .await?;
    let quota_measured = q
        .map(|r| {
            r.i64_or("measured", 0) == 1
                && r.i64_or("used_window", 0) < r.i64_or("limit_per_min", 0)
        })
        .unwrap_or(false);
    let tier = if ok7 >= 3 && ok24 >= 1 && state == "AVAILABLE" && exec_ok >= 1 {
        "VERIFIED"
    } else if ok24 >= 1 && (state == "AVAILABLE" || state == "DEGRADED") && exec_ok >= 1 {
        "AVAILABLE"
    } else if quota_measured {
        "QUOTA"
    } else if free_exec >= 1 {
        "FREE"
    } else if ok7 >= 1 {
        "ACCESSIBLE"
    } else {
        "UNKNOWN"
    };
    Ok(TierInfo { tier: tier.to_string(), ok7, ok24, exec_ok, free_exec, quota_measured })
}

pub async fn warranted_tier(db: &dyn Db, rid: &str) -> Result<String, DbError> {
    Ok(warranted_tier_info(db, rid).await?.tier)
}

pub async fn auto_promote(
    db: &dyn Db,
    rid: &str,
    tier_info: Option<&TierInfo>,
) -> Result<String, DbError> {
    let cur = db
        .query_one("SELECT evidence_state FROM resources WHERE id=?", &[Val::from(rid.to_string())])
        .await
        .map(|o| o.and_then(|r| r.text("evidence_state")).unwrap_or_else(|| "UNKNOWN".to_string()))
        .unwrap_or_else(|_| "UNKNOWN".to_string());
    if cur == "PARKED" {
        return Ok(cur);
    }
    let w = match tier_info {
        Some(ti) => ti.tier.clone(),
        None => warranted_tier(db, rid).await?,
    };
    if ev_rank(&w) > ev_rank(&cur) {
        set_evidence(db, rid, &w, "auto-promote", "warranted by recorded observations", false).await?;
        return Ok(w);
    }
    Ok(cur)
}

/// Notice-and-adapt: usable tiers go stale without fresh success.
pub async fn demote_check(
    db: &dyn Db,
    rid: &str,
    tier_info: Option<&TierInfo>,
) -> Result<Option<String>, DbError> {
    let r = db
        .query_one("SELECT evidence_state FROM resources WHERE id=?", &[Val::from(rid.to_string())])
        .await?;
    let r = match r {
        Some(r) => r,
        None => return Ok(None),
    };
    let ev = r.text_or("evidence_state", "");
    if ev != "AVAILABLE" && ev != "VERIFIED" {
        return Ok(Some(ev));
    }
    let ok24 = match tier_info {
        Some(ti) => ti.ok24,
        None => {
            db.query_one(
                "SELECT COUNT(*) c FROM health_observations WHERE resource_id=? AND ok=1 AND observed_at>?",
                &[Val::from(rid.to_string()), Val::from(minutes_ago(24 * 60))],
            )
            .await
            .map(|o| o.map(|r| r.i64_or("c", 0)).unwrap_or(0))
            .unwrap_or(0)
        }
    };
    if ok24 == 0 {
        let down = if ev == "VERIFIED" { "AVAILABLE" } else { "FREE" };
        set_evidence(db, rid, down, "stale-demote", "no successful observation in 24h", true).await?;
        return Ok(Some(down.to_string()));
    }
    Ok(Some(ev))
}

/// Bounded retention for health_observations (30d, per cron run).
pub async fn prune_health_observations(db: &dyn Db) -> Result<i64, DbError> {
    let cutoff = minutes_ago(30 * 24 * 60);
    let rs = db.query_all("SELECT id FROM resources", &[]).await?;
    let mut pruned = 0;
    for r in rs {
        let n = db
            .exec(
                "DELETE FROM health_observations WHERE resource_id=? AND observed_at<?",
                &[Val::from(r.text_or("id", "")), Val::from(cutoff.clone())],
            )
            .await?;
        pruned += n as i64;
    }
    Ok(pruned)
}

/// Bounded retention for the seal registry (1h replay TTL, 500 rows/run).
pub async fn prune_seal_registry(db: &dyn Db) -> Result<u64, DbError> {
    let cutoff = chrono::Utc::now().timestamp_millis() - 60 * 60 * 1000;
    db.exec(
        "DELETE FROM seal_registry WHERE seal IN (SELECT seal FROM seal_registry WHERE first_seen < ? LIMIT 500)",
        &[Val::from(cutoff)],
    )
    .await
}

/// Notice-and-adapt: billing appeared on a resource we believed was free.
pub async fn billing_check(db: &dyn Db) -> Result<Vec<Value>, DbError> {
    let rows = db
        .query_all("SELECT id,resource,cost,created_at FROM receipts ORDER BY created_at DESC LIMIT 40", &[])
        .await?;
    let mut out = Vec::new();
    for rc in rows {
        let resource = rc.text_or("resource", "");
        let cost = rc.f64("cost").unwrap_or(0.0);
        if resource.is_empty() || !(cost > 0.0) {
            continue;
        }
        if ts(&rc.text_or("created_at", "")) < chrono::Utc::now().timestamp_millis() - 3600000 {
            continue;
        }
        let r = db
            .query_one(
                "SELECT evidence_state,cost_class FROM resources WHERE id=?",
                &[Val::from(resource.clone())],
            )
            .await?;
        let r = match r {
            Some(r) => r,
            None => continue,
        };
        if r.text_or("cost_class", "") == "free"
            && ["FREE", "QUOTA", "AVAILABLE", "VERIFIED"].contains(&r.text_or("evidence_state", "").as_str())
        {
            db.exec(
                "UPDATE resources SET cost_class='paid', cost_basis='measured' WHERE id=?",
                &[Val::from(resource.clone())],
            )
            .await?;
            let ch = set_evidence(
                db,
                &resource,
                "ACCESSIBLE",
                "billing-appeared",
                &format!("receipt {} charged ${} — reachable but not free", rc.text_or("id", ""), cost),
                true,
            )
            .await?;
            let mut m = json!({ "resource": resource, "charged": cost });
            if let (Value::Object(a), Value::Object(bv)) = (&mut m, &ch) {
                for (k, v) in bv {
                    a.insert(k.clone(), v.clone());
                }
            }
            out.push(m);
        }
    }
    Ok(out)
}

/// Probe staleness budget by evidence tier (minutes).
pub fn stale_after_ev(ev: &str) -> i64 {
    match ev {
        "VERIFIED" | "AVAILABLE" => 15,
        "QUOTA" | "FREE" | "ACCESSIBLE" => 60,
        "DISCOVERED" => 180,
        "PARKED" => 1440,
        _ => 360,
    }
}

/// observe: record a health observation and roll up resource state.
///
/// Anti-poisoning: worker-reported failures cap at DEGRADED; QUARANTINE
/// needs a governor-observed failure. RATE_LIMITED/AUTH_ERROR need
/// corroboration before driving disruptive transitions.
pub async fn observe(
    db: &dyn Db,
    resource_id: &str,
    ok: bool,
    latency_ms: f64,
    detail: &str,
) -> Result<Value, DbError> {
    db.exec(
        "INSERT INTO health_observations (resource_id,ok,latency_ms,detail) VALUES (?,?,?,?)",
        &[
            Val::from(resource_id.to_string()),
            Val::from(ok),
            if latency_ms.is_finite() { Val::from(latency_ms) } else { Val::Null },
            Val::from(detail.to_string()),
        ],
    )
    .await?;
    let prev = db
        .query_one("SELECT state FROM resources WHERE id=?", &[Val::from(resource_id.to_string())])
        .await
        .map(|o| o.map(|r| r.text_or("state", "UNKNOWN")).unwrap_or("UNKNOWN".to_string()))
        .unwrap_or("UNKNOWN".to_string());
    let recent = db
        .query_all(
            "SELECT ok, detail FROM health_observations WHERE resource_id=? ORDER BY observed_at DESC LIMIT 3",
            &[Val::from(resource_id.to_string())],
        )
        .await?;
    let mut state: Option<String> = None;
    let all_fail = recent.len() >= 3 && recent.iter().all(|r| r.i64_or("ok", 1) == 0);
    let any_governor = recent.iter().any(|r| !r.text_or("detail", "").starts_with("claimant:"));
    if all_fail && any_governor {
        state = Some("QUARANTINED".to_string());
    } else if all_fail {
        state = Some("DEGRADED".to_string());
    } else if !recent.is_empty() && recent[0].i64_or("ok", 0) == 1 {
        let rate = recent.iter().filter(|r| r.i64_or("ok", 0) == 1).count() as f64 / recent.len() as f64;
        state = Some(if rate >= 0.8 { "AVAILABLE" } else { "DEGRADED" }.to_string());
    }
    let has_class = |cls: &str| !ok && detail.contains(cls);
    let is_worker_reported = detail.starts_with("claimant:");
    // Corroboration: worker-reported signals need 2+ consecutive worker
    // reports or a governor observation agreeing.
    let corroborated = |cls: &str, rows: &[crate::db::Row]| -> bool {
        if !is_worker_reported {
            return true;
        }
        let mut consec = 0;
        for h in rows {
            let d = h.text_or("detail", "");
            if d.starts_with("claimant:") && d.contains(cls) {
                consec += 1;
            } else {
                break;
            }
        }
        if consec >= 2 {
            return true;
        }
        rows.iter().any(|h| {
            let d = h.text_or("detail", "");
            !d.starts_with("claimant:") && d.contains(cls)
        })
    };
    if has_class("RATE_LIMITED") {
        let hist = db
            .query_all(
                "SELECT detail FROM health_observations WHERE resource_id=? ORDER BY observed_at DESC LIMIT 5",
                &[Val::from(resource_id.to_string())],
            )
            .await?;
        if !corroborated("RATE_LIMITED", &hist) {
            db.exec(
                "INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)",
                &[
                    Val::from(resource_id.to_string()),
                    Val::from(format!("health:{}", prev)),
                    Val::from(format!("health:{}", prev)),
                    Val::from("uncorroborated-429"),
                    Val::from("worker-reported RATE_LIMITED without corroboration — recorded, not acted on"),
                ],
            )
            .await?;
        } else {
            let hist10 = db
                .query_all(
                    "SELECT detail FROM health_observations WHERE resource_id=? ORDER BY observed_at DESC LIMIT 10",
                    &[Val::from(resource_id.to_string())],
                )
                .await?;
            let mut consec = 0;
            for h in &hist10 {
                if h.text_or("detail", "").contains("RATE_LIMITED") {
                    consec += 1;
                } else {
                    break;
                }
            }
            let backoff_min = (2i64.pow((consec - 1).max(0) as u32)).min(60);
            let until = (chrono::Utc::now() + chrono::Duration::minutes(backoff_min))
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string();
            db.exec(
                "UPDATE resources SET state='THROTTLED', throttled_until=? WHERE id=?",
                &[Val::from(until.clone()), Val::from(resource_id.to_string())],
            )
            .await?;
            state = Some("THROTTLED".to_string());
            db.exec(
                "INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)",
                &[
                    Val::from(resource_id.to_string()),
                    Val::from(format!("health:{}", prev)),
                    Val::from("health:THROTTLED"),
                    Val::from("throttled-backoff"),
                    Val::from(format!("429 #{} in a row — backing off {}m until {}", consec, backoff_min, until)),
                ],
            )
            .await?;
            return Ok(json!({ "resource_id": resource_id, "state": "THROTTLED", "throttled_until": until }));
        }
    }
    if has_class("AUTH_ERROR") {
        let hist = db
            .query_all(
                "SELECT detail FROM health_observations WHERE resource_id=? ORDER BY observed_at DESC LIMIT 5",
                &[Val::from(resource_id.to_string())],
            )
            .await?;
        if !corroborated("AUTH_ERROR", &hist) {
            db.exec(
                "INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)",
                &[
                    Val::from(resource_id.to_string()),
                    Val::from(format!("health:{}", prev)),
                    Val::from(format!("health:{}", prev)),
                    Val::from("uncorroborated-auth-error"),
                    Val::from("worker-reported AUTH_ERROR without corroboration — recorded, not acted on"),
                ],
            )
            .await?;
        } else {
            db.exec(
                "UPDATE resources SET credential_state='invalid' WHERE id=?",
                &[Val::from(resource_id.to_string())],
            )
            .await?;
            let d200: String = detail.chars().take(200).collect();
            set_evidence(db, resource_id, "PARKED", "credential-invalid", &d200, true).await?;
            return Ok(json!({ "resource_id": resource_id, "state": prev, "credential_disabled": true }));
        }
    }
    if let Some(ref s) = state {
        if s == "AVAILABLE" || s == "DEGRADED" {
            db.exec(
                "UPDATE resources SET state=?, throttled_until=NULL, suspended_until=NULL WHERE id=?",
                &[Val::from(s.clone()), Val::from(resource_id.to_string())],
            )
            .await?;
        } else {
            db.exec(
                "UPDATE resources SET state=? WHERE id=?",
                &[Val::from(s.clone()), Val::from(resource_id.to_string())],
            )
            .await?;
        }
    }
    let final_state = state.clone().unwrap_or(prev.clone());
    if let Some(s) = state {
        if s != prev {
            db.exec(
                "INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)",
                &[
                    Val::from(resource_id.to_string()),
                    Val::from(format!("health:{}", prev)),
                    Val::from(format!("health:{}", s)),
                    Val::from("health-transition"),
                    Val::from(detail.to_string()),
                ],
            )
            .await?;
        }
    }
    if final_state == "QUARANTINED" {
        let r = db
            .query_one(
                "SELECT evidence_state FROM resources WHERE id=?",
                &[Val::from(resource_id.to_string())],
            )
            .await?;
        let ev = r.map(|r| r.text_or("evidence_state", "UNKNOWN")).unwrap_or("UNKNOWN".to_string());
        if ev == "AVAILABLE" || ev == "VERIFIED" {
            set_evidence(db, resource_id, "FREE", "quarantine-demote", "was usable, now unreachable", true).await?;
        } else if ev != "PARKED" && ev != "UNKNOWN" {
            set_evidence(db, resource_id, "UNKNOWN", "quarantine-demote", "never proven; access collapsed", true).await?;
        }
    }
    Ok(json!({ "resource_id": resource_id, "state": final_state }))
}

/// registerCandidate: register a candidate lead (never usable on registration).
pub async fn register_candidate(db: &dyn Db, b: &Value, source: &str) -> Result<Value, DbError> {
    let nm = b
        .get("name")
        .and_then(|v| v.as_str())
        .or_else(|| b.get("provider_name").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();
    let pid = b.get("provider_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if pid.is_empty() || nm.is_empty() {
        return Ok(json!({ "error": "provider_id and name (or provider_name) required" }));
    }
    let rid = b
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            format!("res_{}", pid.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase())
        });
    let notes = b.get("notes").and_then(|v| v.as_str()).unwrap_or("");
    db.exec(
        "INSERT OR IGNORE INTO providers (id,name,kind,state,notes) VALUES (?,?,?,?,?)",
        &[
            Val::from(pid.clone()),
            Val::from(nm.clone()),
            Val::from(b.get("provider_kind").and_then(|v| v.as_str()).unwrap_or("candidate").to_string()),
            Val::from("UNKNOWN"),
            Val::from(format!("{} [DISCOVERED lead — not a resource. Do not route.]", notes)),
        ],
    )
    .await?;
    db.exec(
        "INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)",
        &[
            Val::from(format!("provider:{}", pid)),
            Val::from("UNKNOWN"),
            Val::from("DISCOVERED"),
            Val::from(source.to_string()),
            Val::from(notes.to_string()),
        ],
    )
    .await?;
    if b.get("resource").and_then(|v| v.as_bool()) == Some(false) {
        return Ok(json!({ "discovered": format!("provider:{}", pid), "evidence_state": "DISCOVERED", "usable": false }));
    }
    let cfg = b.get("config").cloned().unwrap_or(json!({}));
    if let Some(adapter) = cfg.get("adapter").and_then(|v| v.as_str()) {
        if !crate::adapters::ADAPTER_NAMES.contains(&adapter) {
            return Ok(json!({ "error": format!("unknown adapter {}", adapter) }));
        }
    }
    db.exec(
        "INSERT OR REPLACE INTO resources (id,provider_id,name,kind,state,cost_class,sovereignty,config,evidence_state,evidence_source,evidence_at,cost_basis) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
        &[
            Val::from(rid.clone()),
            Val::from(pid.clone()),
            Val::from(b.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string()),
            Val::from(b.get("kind").and_then(|v| v.as_str()).unwrap_or("candidate").to_string()),
            Val::from("UNKNOWN"),
            Val::from(b.get("cost_class").and_then(|v| v.as_str()).unwrap_or("unknown").to_string()),
            Val::from(b.get("sovereignty").and_then(|v| v.as_str()).unwrap_or("external").to_string()),
            Val::from(cfg.to_string()),
            Val::from("DISCOVERED"),
            Val::from(source.to_string()),
            Val::from(now_iso()),
            Val::from("unknown"),
        ],
    )
    .await?;
    if let Some(caps) = b.get("capabilities").and_then(|v| v.as_array()) {
        for cap in caps {
            let (name, detail) = match cap {
                Value::String(s) => (s.clone(), json!({})),
                Value::Object(_) => (
                    cap.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    cap.get("detail").cloned().unwrap_or(json!({})),
                ),
                _ => continue,
            };
            db.exec(
                "INSERT OR IGNORE INTO resource_capabilities (resource_id,capability,detail) VALUES (?,?,?)",
                &[Val::from(rid.clone()), Val::from(name), Val::from(detail.to_string())],
            )
            .await?;
        }
    }
    if let Some(rpm) = b.get("rpm_limit").and_then(|v| v.as_i64()) {
        db.exec(
            "INSERT OR REPLACE INTO quotas (resource_id,limit_per_min) VALUES (?,?)",
            &[Val::from(rid.clone()), Val::from(rpm)],
        )
        .await?;
    }
    db.exec(
        "INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)",
        &[
            Val::from(rid.clone()),
            Val::from("UNKNOWN"),
            Val::from("DISCOVERED"),
            Val::from(source.to_string()),
            Val::from(notes.to_string()),
        ],
    )
    .await?;
    Ok(json!({ "discovered": rid, "evidence_state": "DISCOVERED", "usable": false }))
}

/// runDiscovery: pull candidate manifests from configured discovery sources.
pub async fn run_discovery(db: &dyn Db, env: &Env) -> Result<Vec<Value>, DbError> {
    let srcs = db.query_all("SELECT * FROM discovery_sources WHERE enabled=1", &[]).await?;
    let mut out = Vec::new();
    for s in srcs {
        let sid = s.text_or("id", "");
        let last = s.text_or("last_fetched_at", "");
        if !last.is_empty() && ts(&last) > chrono::Utc::now().timestamp_millis() - 24 * 3600 * 1000 {
            out.push(json!({ "source": sid, "skipped": "fresh" }));
            continue;
        }
        let url = s.text_or("url", "");
        match env.http.fetch("GET", &url, &[], None).await {
            Err(e) => {
                db.exec(
                    "UPDATE discovery_sources SET last_result=? WHERE id=?",
                    &[Val::from(format!("fetch failed: {}", e.chars().take(120).collect::<String>())), Val::from(sid.clone())],
                )
                .await?;
                out.push(json!({ "source": sid, "error": e.chars().take(120).collect::<String>() }));
            }
            Ok((status, body)) => {
                if status < 200 || status >= 300 {
                    let msg = format!("HTTP {}", status);
                    db.exec(
                        "UPDATE discovery_sources SET last_result=? WHERE id=?",
                        &[Val::from(format!("fetch failed: {}", msg)), Val::from(sid.clone())],
                    )
                    .await?;
                    out.push(json!({ "source": sid, "error": msg }));
                    continue;
                }
                let data: Value = serde_json::from_str(&body).unwrap_or(json!({}));
                let cands: Vec<Value> = data
                    .get("resources")
                    .or_else(|| data.get("candidates"))
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut added = 0;
                for c in cands.iter().take(50) {
                    let cp = c.get("provider_id").and_then(|v| v.as_str()).unwrap_or("");
                    let cn = c.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    if cp.is_empty() || cn.is_empty() {
                        continue;
                    }
                    let id = c
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| {
                            format!("res_{}", cp.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase())
                        });
                    let exists = db
                        .query_one("SELECT id FROM resources WHERE id=?", &[Val::from(id.clone())])
                        .await?;
                    if exists.is_some() {
                        continue;
                    }
                    let mut c2 = c.clone();
                    c2["id"] = Value::from(id);
                    let r = register_candidate(db, &c2, &format!("discovery:{}", sid)).await?;
                    if r.get("error").is_none() {
                        added += 1;
                    }
                }
                db.exec(
                    "UPDATE discovery_sources SET last_fetched_at=?, last_result=? WHERE id=?",
                    &[Val::from(now_iso()), Val::from(format!("added {} candidate(s)", added)), Val::from(sid.clone())],
                )
                .await?;
                out.push(json!({ "source": sid, "added": added }));
            }
        }
    }
    Ok(out)
}

/// listResources: full resource listing with capabilities, quotas, health.
pub async fn list_resources_impl(db: &dyn Db) -> Result<Vec<Value>, DbError> {
    let rs = db.query_all("SELECT * FROM resources", &[]).await?;
    let mut out = Vec::new();
    for r in rs {
        let rid = r.text_or("id", "");
        let caps = db
            .query_all(
                "SELECT capability,detail FROM resource_capabilities WHERE resource_id=?",
                &[Val::from(rid.clone())],
            )
            .await?;
        let q = db
            .query_one("SELECT * FROM quotas WHERE resource_id=?", &[Val::from(rid.clone())])
            .await?;
        let h = db
            .query_one(
                "SELECT COUNT(*) c, SUM(ok) ok, AVG(latency_ms) lat FROM (SELECT * FROM health_observations WHERE resource_id=? ORDER BY observed_at DESC LIMIT 20)",
                &[Val::from(rid.clone())],
            )
            .await?
            .unwrap_or_default();
        let health = if h.i64_or("c", 0) > 0 {
            let c = h.i64_or("c", 0);
            let ok = h.i64_or("ok", 0);
            json!({ "probes": c,
                    "success_rate": ((ok as f64 / c as f64) * 1000.0).round() / 1000.0,
                    "avg_latency_ms": h.f64("lat").unwrap_or(0.0).round() as i64 })
        } else {
            Value::Null
        };
        out.push(json!({
            "id": rid,
            "provider": r.text_or("provider_id", ""),
            "name": r.text_or("name", ""),
            "kind": r.text_or("kind", ""),
            "state": r.text_or("state", ""),
            "cost_class": r.text_or("cost_class", ""),
            "cost_basis": r.text_or("cost_basis", "unknown"),
            "sovereignty": r.text_or("sovereignty", ""),
            "evidence_state": r.text_or("evidence_state", "UNKNOWN"),
            "evidence_source": r.text("evidence_source"),
            "usable": EVIDENCE_USABLE.contains(&r.text_or("evidence_state", "").as_str()),
            "capabilities": caps.iter().map(|c| Value::from(c.text_or("capability", ""))).collect::<Vec<_>>(),
            "quota": q.map(|qq| json!({
                "limit_per_min": qq.i64("limit_per_min"),
                "used_window": qq.i64("used_window"),
                "basis": if qq.i64_or("measured", 0) == 1 { "measured" } else { "asserted" },
            })).unwrap_or(Value::Null),
            "health": health,
        }));
    }
    Ok(out)
}

/// providerDownCheck: every probeable resource of a provider fails with
/// NETWORK on its latest observation → provider DOWN.
pub async fn provider_down_check(db: &dyn Db) -> Result<Vec<Value>, DbError> {
    let mut out = Vec::new();
    let provs = db.query_all("SELECT * FROM providers", &[]).await?;
    for p in provs {
        let pid = p.text_or("id", "");
        let res = db
            .query_all("SELECT id FROM resources WHERE provider_id=?", &[Val::from(pid.clone())])
            .await?;
        let ids: Vec<String> = res.iter().map(|r| r.text_or("id", "")).collect();
        if ids.is_empty() {
            continue;
        }
        let mut any_probeable = false;
        let mut all_net_fail = true;
        for id in &ids {
            let r = db
                .query_one("SELECT config,kind FROM resources WHERE id=?", &[Val::from(id.clone())])
                .await?
                .unwrap_or_default();
            if crate::adapters::adapter_for(&r).is_none() {
                continue;
            }
            any_probeable = true;
            let last = db
                .query_one(
                    "SELECT ok,detail FROM health_observations WHERE resource_id=? ORDER BY observed_at DESC LIMIT 1",
                    &[Val::from(id.clone())],
                )
                .await?;
            let is_net_fail = match &last {
                Some(l) => l.i64_or("ok", 1) == 0 && l.text_or("detail", "").contains("NETWORK"),
                None => false,
            };
            if !is_net_fail {
                all_net_fail = false;
                break;
            }
        }
        if any_probeable && all_net_fail && p.text_or("state", "") != "DOWN" {
            db.exec("UPDATE providers SET state='DOWN' WHERE id=?", &[Val::from(pid.clone())]).await?;
            for id in &ids {
                set_evidence(
                    db, id, "UNKNOWN", "provider-down",
                    &format!("provider {} unreachable (NETWORK on all probeable resources)", pid),
                    true,
                )
                .await?;
            }
            out.push(json!({ "provider": pid, "down": true }));
        } else if p.text_or("state", "") == "DOWN" && !all_net_fail {
            db.exec("UPDATE providers SET state='UNKNOWN' WHERE id=?", &[Val::from(pid.clone())]).await?;
            out.push(json!({ "provider": pid, "recovered": true }));
        }
    }
    Ok(out)
}
