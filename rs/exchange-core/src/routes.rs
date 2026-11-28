//! HTTP routes — port of worker.js `handle()` + `scheduled()`.
//!
//! 22 routes. The `Request`/`Response` types are host-agnostic; the
//! workers-rs glue translates to/from real HTTP.

use crate::ballot::{ballot_prompt, get_ballot_policy, model_slug, tally_ballot};
use crate::claims::{complete_claimed, delegate_execution, sweep_expired_claims, WorkerIdent};
use crate::db::{row_to_json, Db, DbError, RowExt, Val};
use crate::env::Env;
use crate::govern::{run_job, submit_job, unclaimed_queue_depth, QUEUE_FULL_THRESHOLD};
use crate::migrate::{ensure_migrated, seed};
use crate::observe::{
    auto_promote, billing_check, demote_check, list_resources_impl, observe,
    prune_health_observations, prune_seal_registry, provider_down_check, register_candidate,
    run_discovery, set_evidence, warranted_tier, warranted_tier_info, EVIDENCE_USABLE,
};
use crate::policy::policy_version;
use crate::util::{now_iso, sha256hex as sha256hex_util, ts, uid};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Option<Value>,
}

impl Request {
    pub fn header(&self, name: &str) -> String {
        self.headers.get(&name.to_lowercase()).cloned().unwrap_or_default()
    }
}

pub struct Response {
    pub status: u16,
    pub body: Value,
}

impl Response {
    pub fn json(body: Value, status: u16) -> Self {
        Response { status, body }
    }
}

// --- auth --------------------------------------------------------------------

fn authed(req: &Request, env: &Env) -> Result<(), (u16, String)> {
    let key = match &env.exchange_key {
        Some(k) => k,
        None => return Err((503, "exchange key not configured — fail-closed".to_string())),
    };
    let h = req.header("authorization");
    if h != format!("Bearer {}", key) {
        return Err((401, "unauthorized".to_string()));
    }
    Ok(())
}

async fn auth_worker(
    db: &dyn Db,
    req: &Request,
    body: &Value,
) -> Result<WorkerIdent, (u16, String)> {
    let wid = if !req.header("x-worker-id").is_empty() {
        req.header("x-worker-id")
    } else {
        body.get("worker_id").and_then(|v| v.as_str()).unwrap_or("").to_string()
    };
    let wsec = if !req.header("x-worker-secret").is_empty() {
        req.header("x-worker-secret")
    } else {
        body.get("worker_secret").and_then(|v| v.as_str()).unwrap_or("").to_string()
    };
    if wid.is_empty() || wsec.is_empty() {
        return Err((401, "worker credentials required (x-worker-id + x-worker-secret)".to_string()));
    }
    let w = db
        .query_one("SELECT * FROM workers WHERE worker_id=?", &[Val::from(wid.clone())])
        .await
        .map_err(|e| (500, e.0))?;
    let w = match w {
        Some(w) => w,
        None => return Err((403, "unknown or disabled worker".to_string())),
    };
    if w.i64_or("disabled", 0) != 0 {
        return Err((403, "unknown or disabled worker".to_string()));
    }
    if sha256hex_util(&wsec) != w.text_or("secret_hash", "") {
        return Err((403, "bad worker secret".to_string()));
    }
    db.exec(
        "UPDATE workers SET last_seen_at=? WHERE worker_id=?",
        &[Val::from(now_iso()), Val::from(wid.clone())],
    )
    .await
    .map_err(|e| (500, e.0))?;
    Ok(WorkerIdent {
        worker_id: w.text_or("worker_id", ""),
        trust_class: w.text_or("trust_class", "remote"),
    })
}

// --- envelope verification -----------------------------------------------------

const REPLAY_TTL_MS: i64 = 60 * 60 * 1000;
const REPLAY_MAX_SEEN: usize = 10000;

static SEEN_SEALS: LazyLock<Arc<Mutex<HashMap<String, i64>>>> =
    LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

/// The seal set for this request: the Env-injected set when the host
/// provides one (tests — one registry per test Env, fully isolated),
/// otherwise the per-isolate global above (production; each Workers
/// isolate has its own statics, so the global is per-isolate there).
fn seen_seals(env: &Env) -> Arc<Mutex<HashMap<String, i64>>> {
    env.seen_seals.clone().unwrap_or_else(|| SEEN_SEALS.clone())
}

fn sweep_seen_seals(map: &Mutex<HashMap<String, i64>>, now_ms: i64) {
    if let Ok(mut m) = map.lock() {
        m.retain(|_, t| now_ms - *t <= REPLAY_TTL_MS);
    }
}

/// Test/fresh-isolate helper: clear the in-memory seal set the given
/// Env resolves to. In production each isolate has its own WASM
/// instance (own statics); tests simulate a fresh isolate by clearing
/// their own Env's injected set — never another test's.
#[doc(hidden)]
pub fn clear_seen_seals_for_test(env: &Env) {
    if let Ok(mut m) = seen_seals(env).lock() {
        m.clear();
    }
}

async fn record_envelope_rejection(db: &dyn Db, route: &str, b: &Value, reason: &str) {
    let e = b.get("envelope");
    let claim_id = e
        .and_then(|e| e.get("claim_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| b.get("claim_id").and_then(|v| v.as_str()).map(|s| s.to_string()));
    let actor = e
        .and_then(|e| e.get("actor"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let _ = db
        .exec(
            "INSERT INTO envelope_rejections (at, route, reason, claim_id, actor) VALUES (?,?,?,?,?)",
            &[
                Val::from(now_iso()),
                Val::from(route.to_string()),
                Val::from(reason.chars().take(300).collect::<String>()),
                claim_id.map(Val::from).unwrap_or(Val::Null),
                actor.map(Val::from).unwrap_or(Val::Null),
            ],
        )
        .await;
}

async fn check_replay_db(db: &dyn Db, env: &Env, seal: &str) -> &'static str {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let map = seen_seals(env);
    sweep_seen_seals(&map, now_ms);
    {
        let m = map.lock().unwrap();
        if m.contains_key(seal) {
            return "replay";
        }
        if m.len() >= REPLAY_MAX_SEEN {
            return "full";
        }
    }
    match db
        .exec(
            "INSERT INTO seal_registry (seal, first_seen) VALUES (?, ?) ON CONFLICT(seal) DO NOTHING",
            &[Val::from(seal.to_string()), Val::from(now_ms)],
        )
        .await
    {
        Ok(0) => {
            map.lock().unwrap().insert(seal.to_string(), now_ms);
            "replay"
        }
        Ok(_) => {
            map.lock().unwrap().insert(seal.to_string(), now_ms);
            "ok"
        }
        Err(_) => "unavailable",
    }
}

fn envelope_binding_value(e: &Value, field: &str) -> Option<String> {
    if field == "claim_id" {
        return e.get("claim_id").and_then(|v| v.as_str()).map(|s| s.to_string());
    }
    let p = e.get("payload")?;
    if !p.is_object() {
        return None;
    }
    match field {
        "payload.resource_id" => p.get("resource_id").and_then(|v| v.as_str()).map(|s| s.to_string()),
        "payload.proposition" => p.get("proposition").and_then(|v| v.as_str()).map(|s| s.to_string()),
        _ => None,
    }
}

async fn claim_context_for(db: &dyn Db, route: &str, b: &Value) -> Result<(String, String), (u16, String)> {
    if route == "complete" {
        let mut cid = b.get("claim_id").and_then(|v| v.as_str()).map(|s| s.to_string());
        if cid.is_none() {
            if let Some(lid) = b.get("lease_id").and_then(|v| v.as_str()) {
                let c = db
                    .query_one(
                        "SELECT claim_id FROM claims WHERE lease_id=? AND status='active'",
                        &[Val::from(lid.to_string())],
                    )
                    .await
                    .map_err(|e| (500, e.0))?;
                cid = c.and_then(|r| r.text("claim_id"));
            }
        }
        match cid {
            Some(c) => return Ok(("claim_id".to_string(), c)),
            None => return Err((400, "complete requires claim_id (or a resolvable lease_id)".to_string())),
        }
    }
    if route == "observe" {
        match b.get("resource_id").and_then(|v| v.as_str()) {
            Some(r) => return Ok(("payload.resource_id".to_string(), r.to_string())),
            None => return Err((400, "observe requires resource_id".to_string())),
        }
    }
    if route == "ballots" {
        match b.get("proposition").and_then(|v| v.as_str()) {
            Some(p) => return Ok(("payload.proposition".to_string(), p.to_string())),
            None => return Err((400, "ballots requires proposition".to_string())),
        }
    }
    Err((400, format!("unknown route {:?}", route)))
}

async fn require_envelope(db: &dyn Db, route: &str, b: &Value, env: &Env) -> Result<(), (u16, String)> {
    let key_hex = env.secrets.get("PL_SIG_KEY").map(|s| s.trim().to_string()).unwrap_or_default();
    let valid_key = key_hex.len() == 64 && key_hex.chars().all(|c| c.is_ascii_hexdigit());
    if !valid_key {
        record_envelope_rejection(db, route, b, "authority key not provisioned").await;
        return Err((503, "envelope authority key not provisioned — refusing unverifiable ingest".to_string()));
    }
    let e = b.get("envelope");
    match e {
        Some(Value::Object(_)) => {}
        _ => {
            record_envelope_rejection(db, route, b, "missing envelope").await;
            return Err((400, "missing envelope: receipts must carry a lucidota-receipt/1 envelope".to_string()));
        }
    }
    let e = e.unwrap();
    // Verify via the Rust reference implementation (pl::envelope).
    let key_bytes = (0..32)
        .map(|i| u8::from_str_radix(&key_hex[i * 2..i * 2 + 2], 16).unwrap_or(0))
        .collect::<Vec<_>>();
    let map = e.as_object().unwrap().clone();
    if let Err(err) = pl::envelope::verify_with(&key_bytes, &map) {
        let why: String = err.chars().take(200).collect();
        record_envelope_rejection(db, route, b, &format!("rejected: {}", why)).await;
        return Err((401, format!("envelope rejected: {}", why)));
    }
    let seal = e.get("seal").and_then(|v| v.as_str()).unwrap_or("").to_string();
    match check_replay_db(db, env, &seal).await {
        "replay" => {
            record_envelope_rejection(db, route, b, "replay: envelope seal already seen").await;
            return Err((409, "envelope replay rejected: this envelope was already submitted".to_string()));
        }
        "full" => {
            record_envelope_rejection(db, route, b, "replay registry full — refusing new ingest").await;
            return Err((503, "replay registry full — refusing new ingest (fail-closed)".to_string()));
        }
        "unavailable" => {
            record_envelope_rejection(db, route, b, "replay registry unavailable — refusing new ingest").await;
            return Err((503, "replay registry unavailable — refusing new ingest (fail-closed)".to_string()));
        }
        _ => {}
    }
    // Claim binding: the envelope must attest to THIS request's claim context.
    let (field, expect) = claim_context_for(db, route, b).await.map_err(|(s, msg)| {
        let prefix = if s == 400 { "claim context missing: " } else { "envelope rejected: " };
        (s, format!("{}{}", prefix, msg))
    })?;
    match envelope_binding_value(e, &field) {
        None => {
            let msg = format!("envelope carries no binding for {} (missing {}) — minting-side requirement", route, field);
            record_envelope_rejection(db, route, b, &format!("binding: {}", msg)).await;
            return Err((401, format!("envelope rejected: {}", msg)));
        }
        Some(got) if got != expect => {
            let msg = format!("claim binding mismatch on {}: envelope {:?} != request {:?}", field, got, expect);
            record_envelope_rejection(db, route, b, &format!("binding: {}", msg)).await;
            return Err((401, format!("envelope rejected: {}", msg)));
        }
        _ => {}
    }
    // claim_context_for errors already mapped above; bind them here too.
    Ok(())
}

// --- routes --------------------------------------------------------------------

pub async fn handle(db: &dyn Db, req: &Request, env: &Env) -> Response {
    match handle_inner(db, req, env).await {
        Ok(r) => r,
        Err(e) => Response::json(
            json!({ "error": format!("exchange fault: {}", e.0.chars().take(300).collect::<String>()) }),
            500,
        ),
    }
}

async fn handle_inner(db: &dyn Db, req: &Request, env: &Env) -> Result<Response, DbError> {
    let path = req.path.clone();
    // A3: was migrate() on EVERY request — the D1 burn, unguarded.
    // ensure_migrated pays 1 PK row on a cold isolate, 0 when warm.
    ensure_migrated(db).await?;

    if path == "/" && req.method == "GET" {
        return Ok(Response::json(
            json!({ "name": "lucidota-exchange", "version": crate::VERSION, "truth": "no evidence = UNKNOWN" }),
            200,
        ));
    }
    if path == "/v1/health" && req.method == "GET" {
        let n = db.query_one("SELECT COUNT(*) c FROM resources", &[]).await?;
        // v0.7.0 counts; pre-migration (tables absent) → 0, like the JS
        // try/catch.
        let caps = db
            .query_one("SELECT COUNT(*) c FROM model_capabilities", &[])
            .await
            .map(|o| o.map(|r| r.i64_or("c", 0)).unwrap_or(0))
            .unwrap_or(0);
        let keys = db
            .query_one("SELECT COUNT(*) c FROM key_config", &[])
            .await
            .map(|o| o.map(|r| r.i64_or("c", 0)).unwrap_or(0))
            .unwrap_or(0);
        return Ok(Response::json(
            json!({ "ok": true, "version": crate::VERSION,
                    "resources": n.map(|r| r.i64_or("c", 0)).unwrap_or(0),
                    "capabilities": caps, "keys": keys, "at": now_iso() }),
            200,
        ));
    }

    // ---- Worker Protocol v1 endpoints (worker-scoped auth first) ----
    if path == "/v1/workers/register" && req.method == "POST" {
        if let Err((s, e)) = authed(req, env) {
            return Ok(Response::json(json!({ "error": e }), s));
        }
        let b = match &req.body {
            Some(v) => v.clone(),
            None => return Ok(Response::json(json!({ "error": "invalid JSON" }), 400)),
        };
        let worker_id: String = b.get("worker_id").and_then(|v| v.as_str()).unwrap_or("").chars().take(64).collect();
        if worker_id.is_empty()
            || !worker_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
        {
            return Ok(Response::json(json!({ "error": "worker_id required (alphanumeric, _ . -)" }), 400));
        }
        let trust_class = if b.get("trust_class").and_then(|v| v.as_str()) == Some("local") {
            "local"
        } else {
            "remote"
        };
        let secret = crate::util::mint_worker_secret();
        let exists = db
            .query_one("SELECT worker_id FROM workers WHERE worker_id=?", &[Val::from(worker_id.clone())])
            .await?;
        if exists.is_some() {
            return Ok(Response::json(json!({ "error": "worker_id already registered" }), 409));
        }
        let name: String = b.get("name").and_then(|v| v.as_str()).unwrap_or("").chars().take(120).collect();
        db.exec(
            "INSERT INTO workers (worker_id, secret_hash, trust_class, name) VALUES (?,?,?,?)",
            &[Val::from(worker_id.clone()), Val::from(sha256hex_util(&secret)),
              Val::from(trust_class), Val::from(name.clone())],
        )
        .await?;
        let name80: String = name.chars().take(80).collect();
        db.exec(
            "INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)",
            &[Val::from(format!("worker:{}", worker_id)), Val::from("nonexistent"),
              Val::from("registered"), Val::from("worker-register"),
              Val::from(format!("trust_class={} name={}", trust_class, name80))],
        )
        .await?;
        return Ok(Response::json(
            json!({ "worker_id": worker_id, "worker_secret": secret, "trust_class": trust_class,
                    "note": "secret shown once — the Exchange stores only its sha256", "at": now_iso() }),
            200,
        ));
    }

    if path == "/v1/claim" && req.method == "POST" {
        let b = match &req.body {
            Some(v) => v.clone(),
            None => return Ok(Response::json(json!({ "error": "invalid JSON" }), 400)),
        };
        let worker = match auth_worker(db, req, &b).await {
            Ok(w) => w,
            Err((s, e)) => return Ok(Response::json(json!({ "error": e }), s)),
        };
        let resource_id = b.get("resource_id").and_then(|v| v.as_str()).unwrap_or("");
        if resource_id.is_empty() {
            return Ok(Response::json(json!({ "error": "resource_id required" }), 400));
        }
        let res = db
            .query_one("SELECT * FROM resources WHERE id=?", &[Val::from(resource_id.to_string())])
            .await?;
        let res = match res {
            Some(r) => r,
            None => return Ok(Response::json(json!({ "error": "unknown resource" }), 404)),
        };
        let need = res.text_or("claimant_trust_required", "any");
        if need == "local" && worker.trust_class != "local" {
            return Ok(Response::json(
                json!({ "claimed": false, "error": "resource requires local-trust claimant" }),
                403,
            ));
        }
        // v0.7.2 CLAIM RESUME: a claim response lost to transport
        // (truncated read) leaves the lease claimed server-side while
        // the worker saw an error. The worker retries with resume:true;
        // return ITS live unexpired claim delegation instead of claiming
        // a new lease. A worker holds at most one active claim per
        // resource, so the latest-expiring active claim is the lost
        // one. No live claim of mine → fall through to a fresh claim.
        if b.get("resume") == Some(&Value::Bool(true)) {
            let mine = db
                .query_one(
                    "SELECT c.claim_id, c.job_id, c.lease_id, c.lease_expiry, c.policy_snapshot,
                            c.receipt_requirements, c.idempotency_key, j.spec, e.id AS attempt_id
                     FROM claims c JOIN jobs j ON j.id=c.job_id JOIN leases l ON l.id=c.lease_id
                     LEFT JOIN executions e ON e.claim_id=c.claim_id
                     WHERE c.worker_id=? AND c.resource_id=? AND c.status='active'
                       AND l.state='claimed' AND c.lease_expiry > ?
                     ORDER BY c.lease_expiry DESC LIMIT 1",
                    &[
                        Val::from(worker.worker_id.clone()),
                        Val::from(resource_id.to_string()),
                        Val::from(now_iso()),
                    ],
                )
                .await?;
            if let Some(mine) = mine {
                let rspec: Value =
                    serde_json::from_str(&mine.text_or("spec", "{}")).unwrap_or(json!({}));
                let rconf: Value =
                    serde_json::from_str(&res.text_or("config", "{}")).unwrap_or(json!({}));
                let parse_or_raw = |s: String| -> Value {
                    serde_json::from_str(&s).unwrap_or(Value::String(s))
                };
                return Ok(Response::json(
                    json!({ "claimed": true, "resumed": true,
                        "delegation": {
                            "claim_id": mine.text_or("claim_id", ""),
                            "resource_id": resource_id,
                            "capability": rspec.get("capability").cloned().unwrap_or(Value::Null),
                            "input": rspec.get("input").cloned().unwrap_or(Value::Null),
                            "policy_snapshot": parse_or_raw(mine.text_or("policy_snapshot", "")),
                            "lease_expiry": mine.text_or("lease_expiry", ""),
                            "attempt": val_json(mine.get("attempt_id")),
                            "idempotency_key": val_json(mine.get("idempotency_key")),
                            "receipt_requirements": parse_or_raw(mine.text_or("receipt_requirements", "")),
                            "credential_ref": rconf.get("credential_ref").cloned().unwrap_or(Value::Null),
                            "requested_model": rspec.get("input").and_then(|i| i.get("model"))
                                .and_then(|v| v.as_str()).filter(|s| !s.is_empty())
                                .map(|s| Value::from(s.to_string()))
                                .or_else(|| rconf.get("model").cloned())
                                .unwrap_or(Value::Null),
                            "model_exact": rconf.get("model_exact") != Some(&Value::Bool(false)),
                        },
                        "lease_id": mine.text_or("lease_id", ""), "job_id": mine.text_or("job_id", ""),
                        "attempt_id": val_json(mine.get("attempt_id")), "at": now_iso() }),
                    200,
                ));
            }
        }
        let d = db
            .query_one(
                "SELECT l.id AS lease_id, l.job_id, l.expires_at, j.spec
                 FROM leases l JOIN jobs j ON j.id=l.job_id
                 LEFT JOIN claims c ON c.lease_id=l.id
                 WHERE l.resource_id=? AND l.state='active' AND c.claim_id IS NULL AND j.state='deferred'
                   AND l.expires_at > ?
                 ORDER BY l.created_at ASC LIMIT 1",
                &[Val::from(resource_id.to_string()), Val::from(now_iso())],
            )
            .await?;
        let d = match d {
            Some(d) => d,
            None => return Ok(Response::json(json!({ "claimed": false, "at": now_iso() }), 200)),
        };
        let upd = db
            .exec(
                "UPDATE leases SET state='claimed' WHERE id=? AND state='active' AND expires_at > ?",
                &[Val::from(d.text_or("lease_id", "")), Val::from(now_iso())],
            )
            .await?;
        if upd == 0 {
            return Ok(Response::json(
                json!({ "claimed": false, "note": "lost race or lease not active", "at": now_iso() }),
                200,
            ));
        }
        let spec: Value = serde_json::from_str(&d.text_or("spec", "{}")).unwrap_or(json!({}));
        let policy_v = policy_version(db).await?;
        let claim_id = uid("claim");
        let input_hash = sha256hex_util(&serde_json::to_string(&spec["input"]).unwrap_or_default());
        let policy_snapshot = json!({
            "policy_version": policy_v,
            "max_cost": spec.get("max_cost").and_then(|v| v.as_f64()).unwrap_or(0.0),
            "data_class": spec.get("data_class").and_then(|v| v.as_str()).unwrap_or("public"),
            "spend_authority": spec.get("spend_authority").and_then(|v| v.as_f64()).unwrap_or(0.0),
        });
        let receipt_requirements =
            json!(["output_hash", "model_identity", "latency_ms", "worker_outcome"]);
        let execution_id = uid("exec");
        let attempt_id = format!("{}_a1", execution_id);
        let idempotency_key = spec
            .get("idempotency_key")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| d.text_or("job_id", ""));
        db.batch(vec![
            (
                "INSERT INTO claims (claim_id,job_id,worker_id,lease_id,resource_id,attempt_no,status,lease_expiry,policy_snapshot,receipt_requirements,idempotency_key,input_hash) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)".to_string(),
                vec![
                    Val::from(claim_id.clone()),
                    Val::from(d.text_or("job_id", "")),
                    Val::from(worker.worker_id.clone()),
                    Val::from(d.text_or("lease_id", "")),
                    Val::from(resource_id.to_string()),
                    Val::from(1i64),
                    Val::from("active"),
                    Val::from(d.text_or("expires_at", "")),
                    Val::from(policy_snapshot.to_string()),
                    Val::from(receipt_requirements.to_string()),
                    Val::from(idempotency_key.clone()),
                    Val::from(input_hash.clone()),
                ],
            ),
            (
                "INSERT INTO executions (id,job_id,resource_id,started_at,result,attempt_no,lease_id,request_hash,claim_id) VALUES (?,?,?,?,?,?,?,?,?)".to_string(),
                vec![
                    Val::from(attempt_id.clone()),
                    Val::from(d.text_or("job_id", "")),
                    Val::from(resource_id.to_string()),
                    Val::from(now_iso()),
                    Val::from("running"),
                    Val::from(1i64),
                    Val::from(d.text_or("lease_id", "")),
                    Val::from(input_hash),
                    Val::from(claim_id.clone()),
                ],
            ),
        ])
        .await?;
        let rconf: Value = serde_json::from_str(&res.text_or("config", "{}")).unwrap_or(json!({}));
        return Ok(Response::json(
            json!({ "claimed": true,
                "delegation": {
                    "claim_id": claim_id, "resource_id": resource_id,
                    "capability": spec.get("capability").cloned().unwrap_or(Value::Null),
                    "input": spec.get("input").cloned().unwrap_or(Value::Null),
                    "policy_snapshot": policy_snapshot, "lease_expiry": d.text_or("expires_at", ""),
                    "attempt": attempt_id, "idempotency_key": idempotency_key,
                    "receipt_requirements": receipt_requirements,
                    "credential_ref": rconf.get("credential_ref").cloned().unwrap_or(Value::Null),
                    // v0.7.0: the delegation binds the JOB's requested
                    // model first — the resource default only when the
                    // job pins none. The claimant must execute this
                    // exact model or refuse (MODEL_MISMATCH);
                    // model_exact=false pools serve unpinned jobs only.
                    "requested_model": spec.get("input").and_then(|i| i.get("model"))
                        .and_then(|v| v.as_str()).filter(|s| !s.is_empty())
                        .map(|s| Value::from(s.to_string()))
                        .or_else(|| rconf.get("model").cloned())
                        .unwrap_or(Value::Null),
                    "model_exact": rconf.get("model_exact") != Some(&Value::Bool(false)),
                },
                "lease_id": d.text_or("lease_id", ""), "job_id": d.text_or("job_id", ""),
                "attempt_id": attempt_id, "at": now_iso() }),
            200,
        ));
    }

    if path == "/v1/complete" && req.method == "POST" {
        let b = match &req.body {
            Some(v) => v.clone(),
            None => return Ok(Response::json(json!({ "error": "invalid JSON" }), 400)),
        };
        let worker = match auth_worker(db, req, &b).await {
            Ok(w) => w,
            Err((s, e)) => return Ok(Response::json(json!({ "error": e }), s)),
        };
        if let Err((s, e)) = require_envelope(db, "complete", &b, env).await {
            return Ok(Response::json(json!({ "error": e }), s));
        }
        let r = complete_claimed(db, &b, &worker, env).await?;
        return Ok(Response::json(r.body, r.status));
    }

    if path == "/v1/admin/sweep-expired-claims" && req.method == "POST" {
        if let Err((s, e)) = authed(req, env) {
            return Ok(Response::json(json!({ "error": e }), s));
        }
        let swept = sweep_expired_claims(db).await?;
        return Ok(Response::json(json!({ "swept": swept, "at": now_iso() }), 200));
    }
    // ---- end Worker Protocol v1 endpoints ----

    if let Err((s, e)) = authed(req, env) {
        return Ok(Response::json(json!({ "error": e }), s));
    }

    if path == "/v1/seed" && req.method == "POST" {
        let r = seed(db).await?;
        let mut m = r.as_object().cloned().unwrap_or_default();
        m.insert("at".to_string(), Value::from(now_iso()));
        return Ok(Response::json(Value::Object(m), 200));
    }
    if path == "/v1/resources" && req.method == "GET" {
        let resources = list_resources_impl(db).await?;
        return Ok(Response::json(json!({ "resources": resources, "at": now_iso() }), 200));
    }
    if path == "/v1/resources" && req.method == "POST" {
        let b = match &req.body {
            Some(v) => v.clone(),
            None => return Ok(Response::json(json!({ "error": "invalid JSON" }), 400)),
        };
        let r = register_candidate(db, &b, "api-register").await?;
        if r.get("error").is_some() {
            return Ok(Response::json(json!({ "error": r["error"].clone() }), 400));
        }
        let mut m = r.as_object().cloned().unwrap_or_default();
        m.insert("at".to_string(), Value::from(now_iso()));
        m.insert("note".to_string(), Value::from("DISCOVERED lead — run POST /v1/resources/:id/verify to prove it"));
        return Ok(Response::json(Value::Object(m), 200));
    }
    if path == "/v1/discover" && req.method == "POST" {
        let b = match &req.body {
            Some(v) => v.clone(),
            None => return Ok(Response::json(json!({ "error": "invalid JSON" }), 400)),
        };
        let source = b.get("evidence_source").and_then(|v| v.as_str()).unwrap_or("manual").to_string();
        let r = register_candidate(db, &b, &source).await?;
        if r.get("error").is_some() {
            return Ok(Response::json(json!({ "error": r["error"].clone() }), 400));
        }
        let mut m = r.as_object().cloned().unwrap_or_default();
        m.insert("at".to_string(), Value::from(now_iso()));
        return Ok(Response::json(Value::Object(m), 200));
    }

    if let Some(rid) = match_path(&path, "/v1/resources/", "/evidence") {
        if req.method == "GET" {
            let r = db
                .query_one(
                    "SELECT id,evidence_state,evidence_source,evidence_at,cost_basis,state FROM resources WHERE id=?",
                    &[Val::from(rid.clone())],
                )
                .await?;
            match r {
                None => return Ok(Response::json(json!({ "error": "resource not found" }), 404)),
                Some(r) => {
                    let log = db
                        .query_all(
                            "SELECT at,from_state,to_state,trigger,detail FROM evidence_log WHERE resource_id=? ORDER BY id DESC LIMIT 50",
                            &[Val::from(rid.clone())],
                        )
                        .await?;
                    let usable = EVIDENCE_USABLE.contains(&r.text_or("evidence_state", "").as_str());
                    return Ok(Response::json(
                        json!({ "resource": row_to_json(&r), "usable": usable,
                                "log": log.iter().map(row_to_json).collect::<Vec<_>>(), "at": now_iso() }),
                        200,
                    ));
                }
            }
        }
    }

    if let Some(rid) = match_path(&path, "/v1/resources/", "/verify") {
        if req.method == "POST" {
            return verify_resource(db, &rid, env).await;
        }
    }

    if path == "/v1/capacity" && req.method == "GET" {
        return capacity(db).await;
    }

    // ---- v0.7.0: CAPABILITIES + KNOBS/DIALS ----
    // /v1/capabilities lists every model the Exchange can route, per
    // key, per model — from model_capabilities (live /models
    // enumeration). /v1/config is the knobs: per-key enable, allowlist,
    // cost cap, failover order + the global allow_paid kill-switch.
    // Every change is receipted in config_log + evidence_log. All four
    // routes sit behind the Exchange-key gate above (in handle_inner,
    // `authed` has already run by this point).
    if path == "/v1/capabilities" && req.method == "GET" {
        return capabilities(db, req).await;
    }
    if path == "/v1/config" && req.method == "GET" {
        return get_config(db).await;
    }
    if path == "/v1/config/log" && req.method == "GET" {
        return get_config_log(db, req).await;
    }
    if path == "/v1/config" && req.method == "POST" {
        let b = match &req.body {
            Some(v) => v.clone(),
            None => return Ok(Response::json(json!({ "error": "invalid JSON" }), 400)),
        };
        return post_config(db, &b).await;
    }

    // ---- voting ----
    if path == "/v1/ballots" && req.method == "POST" {
        let b = match &req.body {
            Some(v) => v.clone(),
            None => return Ok(Response::json(json!({ "error": "invalid JSON" }), 400)),
        };
        if let Err((s, e)) = require_envelope(db, "ballots", &b, env).await {
            return Ok(Response::json(json!({ "error": e }), s));
        }
        return submit_ballot(db, &b, env).await;
    }
    if let Some(bid) = match_path(&path, "/v1/ballots/", "") {
        if req.method == "GET" && !bid.is_empty() {
            let ballot = db.query_one("SELECT * FROM ballots WHERE id=?", &[Val::from(bid.clone())]).await?;
            match ballot {
                None => return Ok(Response::json(json!({ "error": "ballot not found" }), 404)),
                Some(bal) => {
                    let mut t = tally_ballot(db, &bal).await?;
                    t["at"] = Value::from(now_iso());
                    return Ok(Response::json(t, 200));
                }
            }
        }
    }
    if path == "/v1/ballot-policy" && req.method == "GET" {
        let pol = get_ballot_policy(db).await?;
        return Ok(Response::json(
            json!({ "policy": { "voters": pol.voters, "quorum_rule": pol.quorum_rule,
                                "deadline_s": pol.deadline_s, "tie_policy": pol.tie_policy },
                    "at": now_iso() }),
            200,
        ));
    }

    if path == "/v1/job" && req.method == "POST" {
        let spec = match &req.body {
            Some(v) => v.clone(),
            None => return Ok(Response::json(json!({ "error": "invalid JSON" }), 400)),
        };
        let r = submit_job(db, &spec, env).await?;
        let status = r.get("status").and_then(|v| v.as_u64()).unwrap_or(200) as u16;
        return Ok(Response::json(r, status));
    }
    if let Some(jid) = match_path(&path, "/v1/job/", "") {
        if req.method == "GET" && !jid.is_empty() {
            let job = db.query_one("SELECT * FROM jobs WHERE id=?", &[Val::from(jid.clone())]).await?;
            match job {
                None => return Ok(Response::json(json!({ "error": "job not found" }), 404)),
                Some(job) => {
                    let execs = db.query_all("SELECT * FROM executions WHERE job_id=? ORDER BY attempt_no", &[Val::from(jid.clone())]).await?;
                    let leases = db.query_all("SELECT * FROM leases WHERE job_id=?", &[Val::from(jid.clone())]).await?;
                    let rcpts = db.query_all("SELECT * FROM receipts WHERE job_id=?", &[Val::from(jid.clone())]).await?;
                    let claims = db.query_all("SELECT claim_id,job_id,worker_id,lease_id,resource_id,attempt_no,status,claimed_at,lease_expiry,worker_outcome,completed_at FROM claims WHERE job_id=?", &[Val::from(jid.clone())]).await?;
                    let accts = db.query_all("SELECT * FROM accounting WHERE job_id=?", &[Val::from(jid.clone())]).await?;
                    return Ok(Response::json(json!({
                        "job": row_to_json(&job),
                        "executions": execs.iter().map(row_to_json).collect::<Vec<_>>(),
                        "attempts": execs.iter().map(row_to_json).collect::<Vec<_>>(),
                        "leases": leases.iter().map(row_to_json).collect::<Vec<_>>(),
                        "claims": claims.iter().map(row_to_json).collect::<Vec<_>>(),
                        "receipts": rcpts.iter().map(row_to_json).collect::<Vec<_>>(),
                        "accounting": accts.iter().map(row_to_json).collect::<Vec<_>>(),
                    }), 200));
                }
            }
        }
    }
    if path == "/v1/jobs" && req.method == "GET" {
        let state = req.query.get("state").cloned();
        let (q, params): (String, Vec<Val>) = match state {
            Some(s) => (
                "SELECT id,organelle,capability,data_class,state,defer_count,created_at FROM jobs WHERE state=? ORDER BY created_at DESC LIMIT 50".to_string(),
                vec![Val::from(s)],
            ),
            None => (
                "SELECT id,organelle,capability,data_class,state,defer_count,created_at FROM jobs ORDER BY created_at DESC LIMIT 50".to_string(),
                vec![],
            ),
        };
        let r = db.query_all(&q, &params).await?;
        return Ok(Response::json(
            json!({ "jobs": r.iter().map(row_to_json).collect::<Vec<_>>(), "at": now_iso() }),
            200,
        ));
    }
    if path == "/v1/receipts" && req.method == "GET" {
        let job = req.query.get("job").cloned();
        let (q, params): (String, Vec<Val>) = match job {
            Some(j) => (
                "SELECT * FROM receipts WHERE job_id=? ORDER BY created_at DESC LIMIT 50".to_string(),
                vec![Val::from(j)],
            ),
            None => ("SELECT * FROM receipts ORDER BY created_at DESC LIMIT 50".to_string(), vec![]),
        };
        let r = db.query_all(&q, &params).await?;
        return Ok(Response::json(
            json!({ "receipts": r.iter().map(row_to_json).collect::<Vec<_>>() }),
            200,
        ));
    }
    if let Some(vid) = match_path(&path, "/v1/receipts/", "/verify") {
        if req.method == "GET" {
            return verify_receipt(db, &vid).await;
        }
    }
    if path == "/v1/observe" && req.method == "POST" {
        let b = match &req.body {
            Some(v) => v.clone(),
            None => return Ok(Response::json(json!({ "error": "invalid JSON" }), 400)),
        };
        if let Err((s, e)) = require_envelope(db, "observe", &b, env).await {
            return Ok(Response::json(json!({ "error": e }), s));
        }
        let rid = b.get("resource_id").and_then(|v| v.as_str()).unwrap_or("");
        if rid.is_empty() {
            return Ok(Response::json(json!({ "error": "resource_id required" }), 400));
        }
        let r = observe(
            db,
            rid,
            b.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
            b.get("latency_ms").and_then(|v| v.as_f64()).unwrap_or(0.0),
            b.get("detail").and_then(|v| v.as_str()).unwrap_or(""),
        )
        .await?;
        return Ok(Response::json(r, 200));
    }

    Ok(Response::json(json!({ "error": "not found" }), 404))
}

/// Convert a single DB value to JSON (mirrors D1 row value semantics).
/// Takes the `HashMap::get` result directly: missing column → Null.
fn val_json(v: Option<&crate::db::Val>) -> Value {
    match v {
        None | Some(crate::db::Val::Null) => Value::Null,
        Some(crate::db::Val::Int(i)) => Value::from(*i),
        Some(crate::db::Val::Real(f)) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Some(crate::db::Val::Text(s)) => Value::from(s.clone()),
        Some(crate::db::Val::Blob(b)) => Value::from(hex::encode(b)),
    }
}

// --- v0.7.0 capabilities + config handlers -----------------------------------

async fn capabilities(db: &dyn Db, req: &Request) -> Result<Response, DbError> {
    let rows = db
        .query_all(
            "SELECT mc.resource_id, mc.model, mc.capability, mc.cost, mc.source, mc.listed_at,
                    mc.last_executed_at, mc.last_error,
                    r.provider_id, r.evidence_state, r.state, r.config
             FROM model_capabilities mc JOIN resources r ON r.id = mc.resource_id
             ORDER BY mc.resource_id, mc.model",
            &[],
        )
        .await?;
    let key_configs = crate::config::load_key_configs(db).await;
    let q_key = req.query.get("key");
    let q_cap = req.query.get("capability");
    let q_cost = req.query.get("cost");
    let q_usable = req.query.get("usable").map(|s| s.as_str()) == Some("1");
    let mut items: Vec<Value> = Vec::new();
    let mut by_cap: serde_json::Map<String, Value> = serde_json::Map::new();
    let mut by_cost: serde_json::Map<String, Value> = serde_json::Map::new();
    let (mut usable_count, mut proven_count) = (0i64, 0i64);
    for row in &rows {
        let cfg: Value = serde_json::from_str(&row.text_or("config", "{}")).unwrap_or(json!({}));
        let key_id = crate::config::key_id_of(&cfg);
        let kc = key_id.as_ref().and_then(|k| key_configs.get(k));
        let key_enabled = kc.map(|k| k.enabled != 0).unwrap_or(true);
        let cost = row.text("cost");
        let usable = EVIDENCE_USABLE.contains(&row.text_or("evidence_state", "").as_str())
            && row.text_or("state", "") != "QUARANTINED"
            && key_enabled
            && cost.as_deref() != Some("unfunded");
        let proven = row.text("last_executed_at").is_some();
        if let Some(qk) = q_key {
            if key_id.as_deref() != Some(qk.as_str()) {
                continue;
            }
        }
        if let Some(qc) = q_cap {
            if row.text_or("capability", "") != *qc {
                continue;
            }
        }
        if let Some(qc) = q_cost {
            if cost.as_deref() != Some(qc.as_str()) {
                continue;
            }
        }
        if q_usable && !usable {
            continue;
        }
        let cur = by_cap.get(&row.text_or("capability", "")).and_then(|v| v.as_i64()).unwrap_or(0);
        by_cap.insert(row.text_or("capability", ""), Value::from(cur + 1));
        let cost_key = cost.clone().unwrap_or_else(|| "null".to_string());
        let cur = by_cost.get(&cost_key).and_then(|v| v.as_i64()).unwrap_or(0);
        by_cost.insert(cost_key, Value::from(cur + 1));
        if usable {
            usable_count += 1;
        }
        if proven {
            proven_count += 1;
        }
        items.push(json!({
            "resource_id": row.text_or("resource_id", ""), "key_id": key_id,
            "provider_id": row.text_or("provider_id", ""),
            "model": row.text_or("model", ""), "capability": row.text_or("capability", ""),
            "cost": val_json(row.get("cost")), "source": val_json(row.get("source")),
            "listed_at": val_json(row.get("listed_at")),
            "last_executed_at": val_json(row.get("last_executed_at")),
            "last_error": val_json(row.get("last_error")),
            "evidence_state": row.text_or("evidence_state", ""), "state": row.text_or("state", ""),
            "key_enabled": key_enabled, "usable": usable, "proven": proven,
        }));
    }
    Ok(Response::json(
        json!({ "at": now_iso(), "total": items.len(), "usable": usable_count, "proven": proven_count,
            "by_capability": by_cap, "by_cost": by_cost, "capabilities": items,
            "note": "Listed is not proven: proven means last_executed_at is set. Usable means the resource is verified, the key is enabled, and the model is funded." }),
        200,
    ))
}

async fn get_config(db: &dyn Db) -> Result<Response, DbError> {
    let kc_rows = db
        .query_all("SELECT * FROM key_config ORDER BY failover_order, key_id", &[])
        .await?;
    let g_rows = db.query_all("SELECT * FROM exchange_config", &[]).await?;
    let log_rows = db
        .query_all("SELECT * FROM config_log ORDER BY id DESC LIMIT 20", &[])
        .await?;
    let keys: Vec<Value> = kc_rows
        .iter()
        .map(|k| {
            let allowlist = match k.get("allowlist") {
                Some(crate::db::Val::Text(s)) => {
                    serde_json::from_str::<Value>(s).unwrap_or(Value::Null)
                }
                _ => Value::Null,
            };
            json!({
                "key_id": k.text_or("key_id", ""),
                "enabled": !matches!(k.get("enabled"), Some(crate::db::Val::Int(0))),
                "allowlist": allowlist,
                "cost_cap": val_json(k.get("cost_cap")),
                "failover_order": val_json(k.get("failover_order")),
                "updated_at": val_json(k.get("updated_at")),
                "updated_by": val_json(k.get("updated_by")),
            })
        })
        .collect();
    let mut global = serde_json::Map::new();
    for g in &g_rows {
        global.insert(g.text_or("key", ""), val_json(g.get("value")));
    }
    Ok(Response::json(
        json!({ "at": now_iso(), "keys": keys, "global": Value::Object(global),
            "recent_changes": log_rows.iter().map(row_to_json).collect::<Vec<_>>() }),
        200,
    ))
}

async fn get_config_log(db: &dyn Db, req: &Request) -> Result<Response, DbError> {
    let rows = match req.query.get("key_id") {
        Some(k) => {
            db.query_all(
                "SELECT * FROM config_log WHERE key_id=? ORDER BY id DESC LIMIT 100",
                &[Val::from(k.clone())],
            )
            .await?
        }
        None => {
            db.query_all("SELECT * FROM config_log ORDER BY id DESC LIMIT 100", &[])
                .await?
        }
    };
    Ok(Response::json(
        json!({ "at": now_iso(), "log": rows.iter().map(row_to_json).collect::<Vec<_>>() }),
        200,
    ))
}

/// Insert a config_log row and return its id (RETURNING — exact id,
/// where the JS reads meta.last_row_id).
async fn config_log_insert(
    db: &dyn Db,
    actor: &str,
    scope: &str,
    key_id: Option<&str>,
    field: &str,
    from: &str,
    to: &str,
    reason: &str,
) -> Result<Option<i64>, DbError> {
    let r = db
        .query_one(
            "INSERT INTO config_log (actor,scope,key_id,field,from_value,to_value,reason) VALUES (?,?,?,?,?,?,?) RETURNING id",
            &[
                Val::from(actor.to_string()),
                Val::from(scope.to_string()),
                key_id.map(|k| Val::from(k.to_string())).unwrap_or(Val::Null),
                Val::from(field.to_string()),
                Val::from(from.to_string()),
                Val::from(to.to_string()),
                Val::from(reason.to_string()),
            ],
        )
        .await?;
    Ok(r.and_then(|row| row.i64("id")))
}

async fn post_config(db: &dyn Db, b: &Value) -> Result<Response, DbError> {
    let actor = "bearer:exchange-key";
    let reason: String = match b.get("reason") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    }
    .chars()
    .take(200)
    .collect();
    let scope = b.get("scope").and_then(|v| v.as_str()).unwrap_or("");
    if scope == "global" {
        if b.get("field").and_then(|v| v.as_str()) != Some("allow_paid") {
            return Ok(Response::json(json!({ "error": "global field must be allow_paid" }), 400));
        }
        let v = b.get("value").cloned().unwrap_or(Value::Null);
        let valid = match &v {
            Value::Number(n) => n.as_f64().map(|f| f == 0.0 || f == 1.0).unwrap_or(false),
            Value::String(s) => s == "0" || s == "1",
            _ => false,
        };
        if !valid {
            return Ok(Response::json(json!({ "error": "allow_paid must be 0 or 1" }), 400));
        }
        let to = match &v {
            Value::String(s) => s.clone(),
            Value::Number(n) => {
                if n.as_f64().map(|f| f == 1.0).unwrap_or(false) { "1".to_string() } else { "0".to_string() }
            }
            _ => unreachable!(),
        };
        let cur = db
            .query_one("SELECT value FROM exchange_config WHERE key='allow_paid'", &[])
            .await?;
        let from = cur.map(|r| r.text_or("value", "0")).unwrap_or_else(|| "0".to_string());
        db.exec(
            "INSERT OR REPLACE INTO exchange_config (key,value,updated_at) VALUES ('allow_paid',?,?)",
            &[Val::from(to.clone()), Val::from(now_iso())],
        )
        .await?;
        let log_id = config_log_insert(db, actor, "global", None, "allow_paid", &from, &to, &reason).await?;
        db.exec(
            "INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)",
            &[
                Val::from("config:global"),
                Val::from(format!("allow_paid={}", from)),
                Val::from(format!("allow_paid={}", to)),
                Val::from("config-change"),
                Val::from(format!(
                    "allow_paid {} -> {} by {}{}",
                    from,
                    to,
                    actor,
                    if reason.is_empty() { String::new() } else { format!(" ({})", reason) }
                )),
            ],
        )
        .await?;
        return Ok(Response::json(
            json!({ "changed": true, "scope": "global", "field": "allow_paid",
                "from": from, "to": to, "log_id": log_id, "at": now_iso(),
                "note": if to == "1" { "PAID EXECUTION ENABLED — paid models may now execute with spend authority" } else { "paid execution disabled" } }),
            200,
        ));
    }
    if scope != "key" {
        return Ok(Response::json(json!({ "error": "scope must be 'key' or 'global'" }), 400));
    }
    let key_id = match b.get("key_id") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    };
    if key_id.is_empty() {
        return Ok(Response::json(json!({ "error": "key_id required" }), 400));
    }
    // The key must exist: at least one resource registered with this key_id.
    let res_rows = db.query_all("SELECT id, config FROM resources", &[]).await?;
    let mut key_exists = false;
    for rr in &res_rows {
        let cfg: Value = serde_json::from_str(&rr.text_or("config", "{}")).unwrap_or(json!({}));
        if cfg.get("key_id").and_then(|v| v.as_str()) == Some(key_id.as_str()) {
            key_exists = true;
            break;
        }
    }
    if !key_exists {
        return Ok(Response::json(
            json!({ "error": format!("unknown key_id {} (no resource registered with this key)", key_id) }),
            400,
        ));
    }
    let cur = db
        .query_one("SELECT * FROM key_config WHERE key_id=?", &[Val::from(key_id.clone())])
        .await?;
    let (mut kc_enabled, mut kc_allowlist, mut kc_cost_cap, mut kc_failover) =
        match &cur {
            Some(row) => (
                row.i64_or("enabled", 1),
                match row.get("allowlist") {
                    Some(crate::db::Val::Text(s)) => Some(s.clone()),
                    _ => None,
                },
                row.f64("cost_cap"),
                row.i64_or("failover_order", 100),
            ),
            None => (1i64, None, None, 100i64),
        };
    let field_in = b.get("field").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut field = field_in.clone();
    let (from, to): (String, String);
    match field_in.as_str() {
        "enabled" => {
            let v = b.get("value").cloned().unwrap_or(Value::Null);
            let as_num: Option<i64> = match &v {
                Value::Number(n) => n.as_i64().filter(|i| *i == 0 || *i == 1),
                Value::Bool(x) => Some(if *x { 1 } else { 0 }),
                _ => None,
            };
            let n = match as_num {
                Some(n) => n,
                None => {
                    return Ok(Response::json(json!({ "error": "enabled must be 0/1" }), 400));
                }
            };
            from = kc_enabled.to_string();
            to = n.to_string();
            kc_enabled = n;
        }
        "allowlist" => {
            let v = b.get("value").cloned().unwrap_or(Value::Null);
            let arr = match v.as_array() {
                Some(a) => a.clone(),
                None => {
                    return Ok(Response::json(
                        json!({ "error": "allowlist must be an array of model ids (or null to clear — send value:null)" }),
                        400,
                    ));
                }
            };
            // Every allowlist entry must be a listed model.
            let mc_rows = db.query_all("SELECT model FROM model_capabilities", &[]).await?;
            let listed: std::collections::HashSet<String> =
                mc_rows.iter().map(|m| m.text_or("model", "")).collect();
            let bad: Vec<String> = arr
                .iter()
                .filter(|m| !m.as_str().map(|s| listed.contains(s)).unwrap_or(false))
                .map(|m| match m {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect();
            if !bad.is_empty() {
                return Ok(Response::json(
                    json!({ "error": format!("allowlist models not listed for any resource: {}", bad.join(", ")) }),
                    400,
                ));
            }
            from = kc_allowlist.clone().unwrap_or_else(|| "null".to_string());
            to = serde_json::to_string(&Value::Array(arr)).unwrap_or_default();
            kc_allowlist = Some(to.clone());
        }
        "allowlist_clear" => {
            from = kc_allowlist.clone().unwrap_or_else(|| "null".to_string());
            to = "null".to_string();
            kc_allowlist = None;
            field = "allowlist".to_string();
        }
        "cost_cap" => {
            let v = b.get("value").cloned().unwrap_or(Value::Null);
            match &v {
                Value::Null => {
                    from = kc_cost_cap.map(|c| js_num_str(c)).unwrap_or_else(|| "null".to_string());
                    to = "null".to_string();
                    kc_cost_cap = None;
                }
                Value::Number(n) => {
                    let f = n.as_f64().unwrap_or(-1.0);
                    if f < 0.0 {
                        return Ok(Response::json(
                            json!({ "error": "cost_cap must be a number >= 0 or null" }),
                            400,
                        ));
                    }
                    from = kc_cost_cap.map(|c| js_num_str(c)).unwrap_or_else(|| "null".to_string());
                    to = js_num_str(f);
                    kc_cost_cap = Some(f);
                }
                _ => {
                    return Ok(Response::json(
                        json!({ "error": "cost_cap must be a number >= 0 or null" }),
                        400,
                    ));
                }
            }
        }
        "failover_order" => {
            let v = b.get("value").cloned().unwrap_or(Value::Null);
            let n = v.as_i64().filter(|i| *i >= 0);
            let n = match n {
                Some(n) => n,
                None => {
                    return Ok(Response::json(
                        json!({ "error": "failover_order must be an integer >= 0" }),
                        400,
                    ));
                }
            };
            from = kc_failover.to_string();
            to = n.to_string();
            kc_failover = n;
        }
        _ => {
            return Ok(Response::json(
                json!({ "error": "field must be enabled, allowlist, allowlist_clear, cost_cap, or failover_order" }),
                400,
            ));
        }
    }
    db.exec(
        "INSERT OR REPLACE INTO key_config (key_id,enabled,allowlist,cost_cap,failover_order,updated_at,updated_by) VALUES (?,?,?,?,?,?,?)",
        &[
            Val::from(key_id.clone()),
            Val::from(kc_enabled),
            kc_allowlist.clone().map(Val::from).unwrap_or(Val::Null),
            kc_cost_cap.map(Val::from).unwrap_or(Val::Null),
            Val::from(kc_failover),
            Val::from(now_iso()),
            Val::from(actor),
        ],
    )
    .await?;
    let log_id = config_log_insert(db, actor, "key", Some(&key_id), &field, &from, &to, &reason).await?;
    db.exec(
        "INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)",
        &[
            Val::from(format!("config:key:{}", key_id)),
            Val::from(format!("{}={}", field, from)),
            Val::from(format!("{}={}", field, to)),
            Val::from("config-change"),
            Val::from(format!(
                "key {} {} {} -> {} by {}{}",
                key_id,
                field,
                from,
                to,
                actor,
                if reason.is_empty() { String::new() } else { format!(" ({})", reason) }
            )),
        ],
    )
    .await?;
    Ok(Response::json(
        json!({ "changed": true, "scope": "key", "key_id": key_id, "field": field,
            "from": from, "to": to, "log_id": log_id, "at": now_iso() }),
        200,
    ))
}

/// JS `String(number)`: integers print without a fraction (5.0 → "5").
fn js_num_str(f: f64) -> String {
    if f == f.trunc() && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        format!("{}", f)
    }
}

/// Match `/v1/resources/<id>/evidence`-style paths. Returns the `<id>`.
fn match_path<'a>(path: &'a str, prefix: &str, suffix: &str) -> Option<String> {
    if !path.starts_with(prefix) {
        return None;
    }
    let rest = &path[prefix.len()..];
    if suffix.is_empty() {
        if rest.is_empty() || rest.contains('/') {
            return None;
        }
        return Some(rest.to_string());
    }
    match rest.strip_suffix(suffix) {
        Some(id) if !id.is_empty() && !id.contains('/') => Some(id.to_string()),
        _ => None,
    }
}

// --- /v1/resources/:id/verify (the evidence gauntlet) --------------------------

async fn verify_resource(db: &dyn Db, rid: &str, env: &Env) -> Result<Response, DbError> {
    let r = db.query_one("SELECT * FROM resources WHERE id=?", &[Val::from(rid.to_string())]).await?;
    let r = match r {
        Some(r) => r,
        None => return Ok(Response::json(json!({ "error": "resource not found" }), 404)),
    };
    let mut chain: Vec<Value> = Vec::new();
    let adapter = crate::adapters::adapter_for(&r);
    if let Some(ref a) = adapter {
        if a.claimed() {
            // v0.7.1: prefer the inference capability for the probe —
            // an unordered caps[0] took embedding first for keys whose
            // embedding row was inserted first, and the gauntlet built
            // an embedding probe with empty input. Inference is the
            // probe that earns the ladder; embedding-only resources
            // still probe embedding.
            let caps = db
                .query_all("SELECT capability FROM resource_capabilities WHERE resource_id=? ORDER BY CASE WHEN capability='inference' THEN 0 ELSE 1 END", &[Val::from(rid.to_string())])
                .await?;
            let cap = caps.first().map(|c| c.text_or("capability", ""));
            match cap {
                None => {
                    chain.push(json!({ "stage": "ACCESSIBLE", "achieved": false,
                        "evidence": "no capabilities registered — nothing to verify" }));
                    return Ok(Response::json(json!({
                        "resource": rid, "evidence_state": r.text_or("evidence_state", ""),
                        "chain": chain, "usable": false, "at": now_iso() }), 200));
                }
                Some(cap) => {
                    let job_id = uid("job");
                    let vinput = if cap == "inference" {
                        json!({ "messages": [{ "role": "user", "content": "Reply with exactly: ok" }], "max_tokens": 64 })
                    } else {
                        json!({})
                    };
                    let vspec = json!({ "organelle": "exchange-verifier", "capability": cap,
                        "input": vinput, "data_class": "public", "max_cost": 0, "priority": 9 });
                    db.exec(
                        "INSERT INTO jobs (id,organelle,capability,input_hash,spec,data_class,max_cost,priority,state,defer_count) VALUES (?,?,?,?,?,?,?,?,?,?)",
                        &[Val::from(job_id.clone()), Val::from("exchange-verifier"), Val::from(cap),
                          Val::from(sha256hex_util(&serde_json::to_string(&vspec["input"]).unwrap_or_default())),
                          Val::from(vspec.to_string()), Val::from("public"), Val::from(0i64),
                          Val::from(9i64), Val::from("submitted"), Val::from(0i64)],
                    )
                    .await?;
                    let provider = db
                        .query_one("SELECT * FROM providers WHERE id=?", &[Val::from(r.text_or("provider_id", ""))])
                        .await?
                        .unwrap_or_default();
                    let vdepth = unclaimed_queue_depth(db, rid, &now_iso()).await?;
                    if vdepth >= QUEUE_FULL_THRESHOLD {
                        db.exec("DELETE FROM jobs WHERE id=?", &[Val::from(job_id.clone())]).await?;
                        chain.push(json!({ "stage": "ACCESSIBLE", "achieved": "deferred",
                            "evidence": format!("queue full (depth {}) — retry after 60s", vdepth) }));
                        return Ok(Response::json(json!({
                            "resource": rid, "evidence_state": r.text_or("evidence_state", ""),
                            "chain": chain, "usable": false, "error": "queue-full",
                            "queue_depth": vdepth, "threshold": QUEUE_FULL_THRESHOLD,
                            "retry_after_s": 60, "at": now_iso() }), 429));
                    }
                    let mut trace: Vec<Value> = Vec::new();
                    let d = delegate_execution(db, &job_id, &vspec, &r, &provider, &mut trace).await?;
                    chain.push(json!({ "stage": "ACCESSIBLE", "achieved": "pending",
                        "evidence": format!("delegated verification job {} — claimant executes the probe", job_id) }));
                    chain.push(json!({ "stage": "FREE", "achieved": "pending",
                        "evidence": "cost measured when the claimant execution completes at $0" }));
                    chain.push(json!({ "stage": "QUOTA", "achieved": "pending",
                        "evidence": "headroom measured from real executions, not pricing pages" }));
                    chain.push(json!({ "stage": "CURRENTLY AVAILABLE", "achieved": "pending",
                        "evidence": "earned by fresh successful executions, not by this call" }));
                    chain.push(json!({ "stage": "VERIFIED", "achieved": "pending",
                        "evidence": "earned by sustained evidence, not by this call" }));
                    return Ok(Response::json(json!({
                        "resource": rid, "evidence_state": r.text_or("evidence_state", ""),
                        "delegated": true, "job_id": job_id, "lease_id": d["lease_id"].clone(),
                        "chain": chain, "usable": false, "at": now_iso(),
                        "note": "gauntlet delegated to claimant; watch GET /v1/resources/:id/evidence for promotion" }), 200));
                }
            }
        }
    }
    let probe_ok = adapter.as_ref().map(|a| !a.claimed()).unwrap_or(false);
    if !probe_ok {
        chain.push(json!({ "stage": "ACCESSIBLE", "achieved": false,
            "evidence": "no probeable adapter/config — remains a DISCOVERED lead, not usable capacity" }));
        return Ok(Response::json(json!({
            "resource": rid, "evidence_state": r.text_or("evidence_state", ""),
            "chain": chain, "usable": false, "at": now_iso() }), 200));
    }
    let adapter = adapter.unwrap();
    // 1. ACCESSIBLE
    let p = adapter.probe(&r, env).await;
    observe(db, rid, p.ok, p.latency_ms, &format!("verify gauntlet: {}", p.detail)).await?;
    chain.push(json!({ "stage": "ACCESSIBLE", "achieved": p.ok,
        "evidence": if p.ok { format!("probe ok ({}ms)", p.latency_ms) } else { format!("probe failed: {}", p.detail) } }));
    if !p.ok {
        let cs = db
            .query_one("SELECT credential_state FROM resources WHERE id=?", &[Val::from(rid.to_string())])
            .await?
            .map(|r| r.text_or("credential_state", ""))
            .unwrap_or_default();
        if cs != "invalid" {
            set_evidence(db, rid, "UNKNOWN", "verify-failed", &p.detail, true).await?;
        } else {
            db.exec(
                "INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)",
                &[Val::from(rid.to_string()), Val::from("PARKED"), Val::from("PARKED"),
                  Val::from("verify-failed"),
                  Val::from("explicit re-verify failed — credential stays invalid/disabled")],
            )
            .await?;
        }
        let cur2 = db
            .query_one("SELECT evidence_state FROM resources WHERE id=?", &[Val::from(rid.to_string())])
            .await?
            .map(|r| r.text_or("evidence_state", ""))
            .unwrap_or_default();
        return Ok(Response::json(json!({
            "resource": rid, "evidence_state": cur2, "chain": chain,
            "usable": false, "at": now_iso() }), 200));
    }
    set_evidence(db, rid, "ACCESSIBLE", "verify-probe", &format!("probe ok, {}ms", p.latency_ms), false).await?;
    db.exec("UPDATE resources SET credential_state='valid' WHERE id=?", &[Val::from(rid.to_string())]).await?;
    // 2. FREE
    db.exec("UPDATE resources SET cost_basis='measured' WHERE id=?", &[Val::from(rid.to_string())]).await?;
    set_evidence(db, rid, "FREE", "verify-economics", "executed real work at measured $0 marginal cost", false).await?;
    chain.push(json!({ "stage": "FREE", "achieved": true,
        "evidence": "real execution completed; cost measured $0 (not claimed)" }));
    // Cold-start bridge: record the gauntlet probe as a real execution.
    let gauntlet_job_id = format!("verify_{}_{}", rid, radix36_now());
    db.exec(
        "INSERT INTO jobs (id,organelle,capability,input_hash,spec,data_class,max_cost,priority,state) VALUES (?,?,?,?,?,?,?,?,?)",
        &[Val::from(gauntlet_job_id.clone()), Val::from("verify-gauntlet"), Val::from("probe"),
          Val::from(sha256hex_util(&format!("verify-gauntlet:{}", rid))),
          Val::from(json!({ "verify": rid, "probe": p.detail }).to_string()),
          Val::from("public"), Val::from(0i64), Val::from(5i64), Val::from("done")],
    )
    .await?;
    db.exec(
        "INSERT INTO executions (id,job_id,resource_id,started_at,finished_at,result,attempt_no,request_hash) VALUES (?,?,?,?,?,?,?,?)",
        &[Val::from(format!("{}_verify", uid("exec"))), Val::from(gauntlet_job_id),
          Val::from(rid.to_string()), Val::from(now_iso()), Val::from(now_iso()),
          Val::from("ok"), Val::from(0i64),
          Val::from(sha256hex_util(&format!("verify-gauntlet:{}:{}", rid, p.detail)))],
    )
    .await?;
    // 3. QUOTA
    let q = db.query_one("SELECT * FROM quotas WHERE resource_id=?", &[Val::from(rid.to_string())]).await?;
    if let Some(qq) = q {
        if qq.i64_or("limit_per_min", 0) > 0 {
            let used = db
                .query_one(
                    "SELECT COUNT(*) c FROM executions WHERE resource_id=? AND result='ok' AND started_at>?",
                    &[Val::from(rid.to_string()), Val::from(crate::util::minutes_ago(1))],
                )
                .await?
                .map(|r| r.i64_or("c", 0))
                .unwrap_or(0);
            let headroom = qq.i64_or("limit_per_min", 0) - used;
            if headroom > 0 {
                db.exec(
                    "UPDATE quotas SET measured=1, measured_at=?, used_window=? WHERE resource_id=?",
                    &[Val::from(now_iso()), Val::from(used), Val::from(rid.to_string())],
                )
                .await?;
                set_evidence(db, rid, "QUOTA", "verify-quota",
                    &format!("headroom measured: {}/{} per min", headroom, qq.i64_or("limit_per_min", 0)), false).await?;
                chain.push(json!({ "stage": "QUOTA", "achieved": true,
                    "evidence": format!("measured headroom {}/{} per min", headroom, qq.i64_or("limit_per_min", 0)) }));
            } else {
                chain.push(json!({ "stage": "QUOTA", "achieved": false,
                    "evidence": format!("no headroom right now ({}/{} used)", used, qq.i64_or("limit_per_min", 0)) }));
            }
        } else {
            chain.push(json!({ "stage": "QUOTA", "achieved": false,
                "evidence": "no quota row — limit unknown, cannot claim headroom" }));
        }
    } else {
        chain.push(json!({ "stage": "QUOTA", "achieved": false,
            "evidence": "no quota row — limit unknown, cannot claim headroom" }));
    }
    // 4-5. CURRENTLY AVAILABLE → VERIFIED
    let w = warranted_tier(db, rid).await?;
    if w == "AVAILABLE" || w == "VERIFIED" {
        set_evidence(db, rid, &w, "verify-sustained", "recent successful executions on record", false).await?;
    }
    let avail = w == "AVAILABLE" || w == "VERIFIED";
    chain.push(json!({ "stage": "CURRENTLY AVAILABLE", "achieved": avail,
        "evidence": if avail { "fresh successful executions on record" } else { "no fresh successful execution — not currently available" } }));
    chain.push(json!({ "stage": "VERIFIED", "achieved": w == "VERIFIED",
        "evidence": if w == "VERIFIED" { "sustained: 3+ successes/7d, fresh probe, healthy" } else { "sustained evidence not yet on record" } }));
    let cur = db
        .query_one("SELECT evidence_state FROM resources WHERE id=?", &[Val::from(rid.to_string())])
        .await?
        .map(|r| r.text_or("evidence_state", ""))
        .unwrap_or_default();
    let usable = EVIDENCE_USABLE.contains(&cur.as_str());
    Ok(Response::json(
        json!({ "resource": rid, "evidence_state": cur, "chain": chain,
                "usable": usable, "at": now_iso() }),
        200,
    ))
}

fn radix36_now() -> String {
    let ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    if ms == 0 {
        return "0".to_string();
    }
    let chars = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    let mut n = ms;
    while n > 0 {
        out.push(chars[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

// --- /v1/capacity ---------------------------------------------------------------

async fn capacity(db: &dyn Db) -> Result<Response, DbError> {
    let rs = db
        .query_all("SELECT * FROM resources WHERE evidence_state IN ('AVAILABLE','VERIFIED')", &[])
        .await?;
    let mut items: Vec<Value> = Vec::new();
    let (mut measured_headroom, mut asserted_headroom) = (0i64, 0i64);
    for r in rs {
        let rid = r.text_or("id", "");
        let q = db.query_one("SELECT * FROM quotas WHERE resource_id=?", &[Val::from(rid.clone())]).await?;
        let ok24 = db
            .query_one(
                "SELECT COUNT(*) c FROM health_observations WHERE resource_id=? AND ok=1 AND observed_at>?",
                &[Val::from(rid.clone()), Val::from(crate::util::minutes_ago(24 * 60))],
            )
            .await?
            .map(|r| r.i64_or("c", 0))
            .unwrap_or(0);
        let headroom: Option<i64> = q.as_ref().and_then(|qq| {
            let l = qq.i64_or("limit_per_min", 0);
            if l > 0 { Some((l - qq.i64_or("used_window", 0)).max(0)) } else { None }
        });
        if let Some(h) = headroom {
            if q.as_ref().map(|qq| qq.i64_or("measured", 0) == 1).unwrap_or(false) {
                measured_headroom += h;
            } else {
                asserted_headroom += h;
            }
        }
        items.push(json!({
            "id": rid, "kind": r.text_or("kind", ""), "evidence_state": r.text_or("evidence_state", ""),
            "cost_class": r.text_or("cost_class", ""), "cost_basis": r.text_or("cost_basis", "unknown"),
            "quota": q.map(|qq| json!({
                "limit_per_min": qq.i64_or("limit_per_min", 0),
                "used_window": qq.i64_or("used_window", 0),
                "headroom": headroom,
                "basis": if qq.i64_or("measured", 0) == 1 { "measured" } else { "asserted" },
            })).unwrap_or(Value::Null),
            "success_24h": ok24,
        }));
    }
    Ok(Response::json(
        json!({ "at": now_iso(), "usable_resources": items.len(),
                "measured_headroom_per_min": measured_headroom,
                "asserted_headroom_per_min": asserted_headroom,
                "items": items,
                "note": "Only AVAILABLE/VERIFIED resources count. A provider listing is not capacity." }),
        200,
    ))
}

// --- /v1/ballots (submit) ----------------------------------------------------------

async fn submit_ballot(db: &dyn Db, b: &Value, env: &Env) -> Result<Response, DbError> {
    let btype = b.get("ballot_type").and_then(|v| v.as_str()).unwrap_or("");
    if btype != "verdict" && btype != "answer" {
        return Ok(Response::json(json!({ "error": "ballot_type must be verdict|answer" }), 400));
    }
    let prop = b.get("proposition").and_then(|v| v.as_str()).unwrap_or("");
    if prop.is_empty() || prop.len() > 4000 {
        return Ok(Response::json(json!({ "error": "proposition required (max 4000 chars)" }), 400));
    }
    let pol = get_ballot_policy(db).await?;
    let raw_voters: Vec<String> = match b.get("voters") {
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect(),
        None => pol.voters.clone(),
        _ => vec![],
    };
    if b.get("voters").is_some() && !b.get("voters").map(|v| v.is_array()).unwrap_or(false) {
        return Ok(Response::json(json!({ "error": "voters: array of 2-7 model ids" }), 400));
    }
    if raw_voters.len() < 2 || raw_voters.len() > 7 {
        return Ok(Response::json(json!({ "error": "voters: array of 2-7 model ids" }), 400));
    }
    let mut seen = std::collections::HashSet::new();
    let mut voters: Vec<String> = Vec::new();
    for v in &raw_voters {
        if v.trim().is_empty() || v.len() > 120 {
            return Ok(Response::json(
                json!({ "error": "each voter must be a non-empty model id string (max 120 chars)" }),
                400,
            ));
        }
        let slug = model_slug(v);
        if seen.contains(&slug) {
            return Ok(Response::json(
                json!({ "error": format!("duplicate voter (by model slug): {}", v) }),
                400,
            ));
        }
        seen.insert(slug);
        voters.push(v.clone());
    }
    if voters.len() < 2 {
        return Ok(Response::json(json!({ "error": "at least 2 distinct voters required" }), 400));
    }
    let depth = unclaimed_queue_depth(db, "nim-nvidia", &now_iso()).await?;
    if depth + voters.len() as i64 > QUEUE_FULL_THRESHOLD {
        return Ok(Response::json(
            json!({ "error": "queue-full", "queue_depth": depth, "needed": voters.len(),
                    "threshold": QUEUE_FULL_THRESHOLD, "retry_after_s": 60, "status": 429 }),
            429,
        ));
    }
    let ballot_id = uid("bal");
    let quorum = voters.len() as i64 / 2 + 1;
    let deadline_s = match b.get("deadline_s") {
        Some(Value::Null) | None => pol.deadline_s,
        Some(v) => v.as_i64().unwrap_or(-1),
    };
    // JS: `typeof deadline_s !== "number" || !Number.isFinite(deadline_s) || deadline_s <= 0` → 400.
    // JSON numbers from a parsed body are always finite; non-numbers → -1 → 400.
    if deadline_s <= 0 {
        return Ok(Response::json(
            json!({ "error": "deadline_s must be a finite positive number of seconds" }),
            400,
        ));
    }
    if deadline_s > 30 * 86400 {
        return Ok(Response::json(json!({ "error": "deadline_s exceeds 30-day maximum" }), 400));
    }
    let deadline_at = (chrono::Utc::now() + chrono::Duration::seconds(deadline_s))
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    db.exec(
        "INSERT INTO ballots (id,ballot_type,proposition,voters,quorum,deadline_at) VALUES (?,?,?,?,?,?)",
        &[
            Val::from(ballot_id.clone()),
            Val::from(btype.to_string()),
            Val::from(prop.to_string()),
            Val::from(serde_json::to_string(&voters).unwrap_or_default()),
            Val::from(quorum),
            Val::from(deadline_at.clone()),
        ],
    )
    .await?;
    let mut jobs: Vec<Value> = Vec::new();
    for model in &voters {
        let prompt = ballot_prompt(btype, prop);
        let vspec = json!({
            "organelle": "ballot-voter", "capability": "inference",
            "input": { "model": model, "messages": [{ "role": "user", "content": prompt }],
                       "temperature": 0, "max_tokens": 256, "ballot_type": btype },
            "ballot_id": ballot_id,
            "data_class": "public", "max_cost": 0, "priority": 6,
            "idempotency_key": format!("ballot_{}_{}", ballot_id, model_slug(model)),
        });
        let job_id = format!("job_{}", vspec["idempotency_key"].as_str().unwrap_or(""));
        let input_hash = sha256hex_util(&serde_json::to_string(&vspec["input"]).unwrap_or_default());
        db.exec(
            "INSERT INTO jobs (id,organelle,capability,input_hash,spec,data_class,max_cost,priority,state,defer_count) VALUES (?,?,?,?,?,?,?,?,?,?)",
            &[Val::from(job_id.clone()), Val::from("ballot-voter"), Val::from("inference"),
              Val::from(input_hash), Val::from(vspec.to_string()), Val::from("public"),
              Val::from(0i64), Val::from(6i64), Val::from("submitted"), Val::from(0i64)],
        )
        .await?;
        let rr = run_job(db, &job_id, &vspec, env, false).await?;
        let st = rr.get("state").and_then(|v| v.as_str()).map(|s| s.to_string())
            .or_else(|| rr.get("error").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .unwrap_or_default();
        jobs.push(json!({ "model": model, "job_id": job_id, "state": st }));
    }
    db.exec(
        "INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)",
        &[Val::from(format!("ballot:{}", ballot_id)), Val::from("none"), Val::from("open"),
          Val::from("ballot-submit"),
          Val::from(format!("{} ballot, {} voters, quorum {}", btype, voters.len(), quorum))],
    )
    .await?;
    Ok(Response::json(
        json!({ "ballot_id": ballot_id, "ballot_type": btype, "state": "open",
                "voters": voters, "quorum": quorum, "deadline_at": deadline_at,
                "jobs": jobs, "at": now_iso() }),
        200,
    ))
}

// --- /v1/receipts/:id/verify -----------------------------------------------------------

async fn verify_receipt(db: &dyn Db, vid: &str) -> Result<Response, DbError> {
    let rcpt = db.query_one("SELECT * FROM receipts WHERE id=?", &[Val::from(vid.to_string())]).await?;
    let rcpt = match rcpt {
        Some(r) => r,
        None => return Ok(Response::json(json!({ "error": "receipt not found" }), 404)),
    };
    let job = db
        .query_one("SELECT * FROM jobs WHERE id=?", &[Val::from(rcpt.text_or("job_id", ""))])
        .await?;
    let mut checks: Vec<Value> = Vec::new();
    match &job {
        Some(j) => {
            let spec: Value = serde_json::from_str(&j.text_or("spec", "{}")).unwrap_or(json!({}));
            let expect = sha256hex_util(&serde_json::to_string(&spec["input"]).unwrap_or_default());
            let actual = rcpt.text_or("input_hash", "");
            checks.push(json!({ "check": "input_hash", "pass": expect == actual,
                "expected": format!("{}…", &expect[..expect.len().min(16)]),
                "actual": format!("{}…", &actual[..actual.len().min(16)]) }));
        }
        None => checks.push(json!({ "check": "input_hash", "pass": false, "reason": "job row missing" })),
    }
    let dec0: Value = serde_json::from_str(&rcpt.text_or("decision", "{}")).unwrap_or(json!({}));
    match dec0.get("output") {
        Some(out) if !out.is_null() && !rcpt.text_or("output_hash", "").is_empty() => {
            let rehash = sha256hex_util(&serde_json::to_string(out).unwrap_or_default());
            checks.push(json!({ "check": "output_hash", "pass": rehash == rcpt.text_or("output_hash", ""),
                "expected": format!("{}…", &rcpt.text_or("output_hash", "")[..rcpt.text_or("output_hash", "").len().min(16)]),
                "actual": format!("{}…", &rehash[..rehash.len().min(16)]) }));
        }
        _ => {
            if serde_json::from_str::<Value>(&rcpt.text_or("decision", "")).is_err() {
                checks.push(json!({ "check": "output_hash", "pass": false, "reason": "decision unparsable" }));
            }
        }
    }
    let (decision, trace_ok) = match serde_json::from_str::<Value>(&rcpt.text_or("decision", "{}")) {
        Ok(d) => {
            let ok = d.get("trace").and_then(|t| t.as_array()).map(|a| !a.is_empty()).unwrap_or(false);
            (d, ok)
        }
        Err(_) => (json!({}), false),
    };
    checks.push(json!({ "check": "governor_trace_present", "pass": trace_ok }));
    let mut auth_trace: Vec<Value> = decision
        .get("trace")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();
    let is_delegated = decision.get("delegated").and_then(|v| v.as_bool()).unwrap_or(false)
        && rcpt.text_or("result", "") == "ok";
    if is_delegated {
        let siblings = db
            .query_all(
                "SELECT id,decision FROM receipts WHERE job_id=? AND id!=?",
                &[Val::from(rcpt.text_or("job_id", "")), Val::from(rcpt.text_or("id", ""))],
            )
            .await?;
        let mut resolved = false;
        for s in siblings {
            if let Ok(sd) = serde_json::from_str::<Value>(&s.text_or("decision", "{}")) {
                let has_lease = sd
                    .get("trace")
                    .and_then(|t| t.as_array())
                    .map(|a| a.iter().any(|g| g["gate"] == "LEASE"))
                    .unwrap_or(false);
                if sd.get("delegated").and_then(|v| v.as_bool()).unwrap_or(false) && has_lease {
                    if let Some(t) = sd.get("trace").and_then(|t| t.as_array()) {
                        auth_trace = t.clone();
                        resolved = true;
                        break;
                    }
                }
            }
        }
        let same_as_decision = auth_trace
            == decision.get("trace").and_then(|t| t.as_array()).cloned().unwrap_or_default();
        checks.push(json!({ "check": "delegation_chain_resolved", "pass": resolved && !same_as_decision,
            "note": "governor trace from delegation receipt" }));
        let cl = if !rcpt.text_or("claim_id", "").is_empty() {
            db.query_one(
                "SELECT worker_id,status FROM claims WHERE claim_id=?",
                &[Val::from(rcpt.text_or("claim_id", ""))],
            )
            .await?
        } else {
            None
        };
        let dec_w = decision.get("worker").and_then(|v| v.as_str());
        let claim_pass = match &cl {
            Some(c) => {
                c.text_or("status", "") == "completed"
                    && dec_w.map(|w| c.text_or("worker_id", "") == w).unwrap_or(true)
            }
            None => false,
        };
        checks.push(json!({ "check": "claim_binding", "pass": claim_pass,
            "claim": rcpt.text_or("claim_id", ""),
            "worker": cl.map(|c| c.text_or("worker_id", "")).unwrap_or_default() }));
        let ac = if !rcpt.text_or("claim_id", "").is_empty() {
            db.query_one(
                "SELECT measured_cost,cost_basis FROM accounting WHERE claim_id=?",
                &[Val::from(rcpt.text_or("claim_id", ""))],
            )
            .await?
        } else {
            None
        };
        let acct_pass = match &ac {
            Some(a) => (a.f64("measured_cost").unwrap_or(-1.0) - rcpt.f64("cost").unwrap_or(-2.0)).abs() < 1e-9,
            None => false,
        };
        checks.push(json!({ "check": "accounting_bound", "pass": acct_pass,
            "measured_cost": ac.as_ref().and_then(|a| a.f64("measured_cost")),
            "cost_basis": ac.as_ref().map(|a| a.text_or("cost_basis", "")) }));
    }
    let gates = ["IDENTITY", "AUTHORITY", "DATA", "CAPABILITY", "EVIDENCE", "QUOTA", "COST", "INTEGRITY", "LEASE", "EFFICIENCY"];
    let present: Vec<String> = auth_trace
        .iter()
        .filter_map(|g| g["gate"].as_str().map(|s| s.to_string()))
        .collect();
    let missing: Vec<&str> = gates.iter().filter(|g| !present.contains(&g.to_string())).copied().collect();
    checks.push(json!({ "check": "all_gates_recorded", "pass": missing.is_empty(), "missing": missing }));
    let max_cost = job
        .as_ref()
        .and_then(|j| serde_json::from_str::<Value>(&j.text_or("spec", "{}")).ok())
        .and_then(|s| s.get("max_cost").and_then(|v| v.as_f64()))
        .unwrap_or(0.0);
    checks.push(json!({ "check": "cost_within_policy",
        "pass": rcpt.f64("cost").unwrap_or(0.0) <= max_cost, "cost": rcpt.f64("cost") }));
    checks.push(json!({ "check": "policy_version_recorded", "pass": !rcpt.text_or("policy_version", "").is_empty(),
        "policy_version": if rcpt.text_or("policy_version", "").is_empty() { Value::Null } else { Value::from(rcpt.text_or("policy_version", "")) } }));
    let fb_ok = match rcpt.text("fallback_chain") {
        None => true,
        Some(s) => serde_json::from_str::<Value>(&s).map(|v| v.is_array()).unwrap_or(false),
    };
    checks.push(json!({ "check": "fallback_chain_structured", "pass": fb_ok }));
    if rcpt.text_or("result", "") == "ok" {
        let lease_ok = serde_json::from_str::<Value>(&rcpt.text_or("lease", "null"))
            .map(|l| l.get("id").is_some() && l.get("expires_at").is_some())
            .unwrap_or(false);
        checks.push(json!({ "check": "lease_snapshot_present", "pass": lease_ok }));
        checks.push(json!({ "check": "attempt_id_present", "pass": !rcpt.text_or("attempt_id", "").is_empty() }));
        let exec_pass = if is_delegated {
            decision
                .get("trace")
                .and_then(|t| t.as_array())
                .map(|a| a.iter().any(|g| g["gate"] == "VERIFY" && g["pass"].as_bool().unwrap_or(false)))
                .unwrap_or(false)
        } else {
            present.contains(&"EXECUTE".to_string())
                && decision
                    .get("trace")
                    .and_then(|t| t.as_array())
                    .map(|a| a.iter().any(|g| g["gate"] == "EXECUTE" && g["pass"].as_bool().unwrap_or(false)))
                    .unwrap_or(false)
        };
        checks.push(json!({ "check": "execution_verified", "pass": exec_pass }));
    }
    let pass = checks.iter().all(|c| c["pass"].as_bool().unwrap_or(false));
    Ok(Response::json(
        json!({ "receipt_id": rcpt.text_or("id", ""), "job_id": rcpt.text_or("job_id", ""),
                "verified": pass, "checks": checks, "at": now_iso() }),
        200,
    ))
}

// --- scheduled() (cron reaction loop) -------------------------------------------------

/// scheduled: the 30-min reaction loop. Returns the cron output object.
pub async fn scheduled(db: &dyn Db, env: &Env) -> Result<Value, DbError> {
    ensure_migrated(db).await?;
    let now_iso_s = now_iso();
    let rs = db.query_all("SELECT * FROM resources", &[]).await?;
    let mut out: Vec<Value> = Vec::new();
    let mut tier_memo: HashMap<String, crate::observe::TierInfo> = HashMap::new();
    for r in rs {
        let rid = r.text_or("id", "");
        let cfg: Value = serde_json::from_str(&r.text_or("config", "{}")).unwrap_or(json!({}));
        let adapter_name = cfg.get("adapter").and_then(|v| v.as_str()).map(|s| s.to_string())
            .unwrap_or_else(|| r.text_or("kind", ""));
        if adapter_name == "self" {
            let ev = auto_promote(db, &rid, None).await?;
            out.push(json!({ "resource": rid, "self": true, "ev": ev }));
            continue;
        }
        if adapter_name == "claimed" {
            let ti = match tier_memo.remove(&rid) {
                Some(t) => t,
                None => warranted_tier_info(db, &rid).await?,
            };
            let prom = auto_promote(db, &rid, Some(&ti)).await?;
            let dev = demote_check(db, &rid, Some(&ti)).await?;
            tier_memo.insert(rid.clone(), ti);
            out.push(json!({ "resource": rid, "skipped": "claimed — claimant observes",
                             "ev": dev.or(Some(prom)) }));
            continue;
        }
        let last = db
            .query_one(
                "SELECT observed_at FROM health_observations WHERE resource_id=? ORDER BY observed_at DESC LIMIT 1",
                &[Val::from(rid.clone())],
            )
            .await?;
        let age_min = last
            .map(|l| (chrono::Utc::now().timestamp_millis() - ts(&l.text_or("observed_at", ""))) as f64 / 60000.0)
            .unwrap_or(9999.0);
        let ev = r.text_or("evidence_state", "UNKNOWN");
        let throttle_lifted = r.text_or("state", "") == "THROTTLED"
            && (r.text_or("throttled_until", "").is_empty() || r.text_or("throttled_until", "") <= now_iso_s);
        let exhaust_lifted = r.text_or("state", "") == "EXHAUSTED"
            && (r.text_or("suspended_until", "").is_empty() || r.text_or("suspended_until", "") <= now_iso_s);
        let needs_probe = ["UNKNOWN", "DEGRADED", "QUARANTINED"].contains(&r.text_or("state", "").as_str())
            || throttle_lifted
            || exhaust_lifted
            || age_min > crate::observe::stale_after_ev(&ev) as f64;
        if !needs_probe {
            let dev = demote_check(db, &rid, None).await?;
            out.push(json!({ "resource": rid, "skipped": "fresh",
                             "ev": dev.unwrap_or(ev), "state": r.text_or("state", "") }));
            continue;
        }
        let adapter = crate::adapters::adapter_for(&r);
        match adapter {
            None => {
                out.push(json!({ "resource": rid, "skipped": "not probeable", "ev": ev }));
                continue;
            }
            Some(a) => {
                // governor-promotion-fix (2026-11-24): worker.js wraps this
                // probe block in try/catch per resource — a fault records an
                // error entry and the loop continues to the next resource.
                // The Rust port propagated every error with `?`, so one
                // faulting resource aborted the whole cron run and froze
                // promotion/demotion for every resource after it. Parity
                // restored: isolate the block, record, continue.
                let probe_block: Result<(), DbError> = (async {
                    // probe() on claimed never happens here (handled above).
                    let p = a.probe(&r, env).await;
                    let o = observe(db, &rid, p.ok, p.latency_ms, &p.detail).await?;
                    if p.ok {
                        let cs = db
                            .query_one("SELECT credential_state FROM resources WHERE id=?", &[Val::from(rid.clone())])
                            .await?
                            .map(|r| r.text_or("credential_state", ""))
                            .unwrap_or_default();
                        if (ev == "DISCOVERED" || ev == "UNKNOWN" || ev == "PARKED") && cs != "invalid" {
                            set_evidence(db, &rid, "ACCESSIBLE", "cron-probe",
                                &format!("probe ok {}ms — auto-ingested", p.latency_ms), false).await?;
                        }
                        let ti = match tier_memo.remove(&rid) {
                            Some(t) => t,
                            None => warranted_tier_info(db, &rid).await?,
                        };
                        auto_promote(db, &rid, Some(&ti)).await?;
                        tier_memo.insert(rid.clone(), ti);
                    }
                    let dev = demote_check(db, &rid, None).await?;
                    let cur = db
                        .query_one("SELECT evidence_state,state FROM resources WHERE id=?", &[Val::from(rid.clone())])
                        .await?
                        .unwrap_or_default();
                    out.push(json!({ "resource": rid, "ok": p.ok, "latency_ms": p.latency_ms,
                                     "state": cur.text_or("state", ""), "ev": cur.text_or("evidence_state", "") }));
                    let _ = (o, dev);
                    Ok(())
                }).await;
                if let Err(e) = probe_block {
                    out.push(json!({ "resource": rid, "ok": false,
                                     "error": e.0.chars().take(120).collect::<String>() }));
                }
            }
        }
    }
    let providers = provider_down_check(db).await?;
    // REAPER
    let mut reaped: Vec<String> = Vec::new();
    let stale = db
        .query_all("SELECT * FROM jobs WHERE state='running' AND created_at < datetime('now','-10 minutes') LIMIT 5", &[])
        .await?;
    for j in stale {
        let jid = j.text_or("id", "");
        let rc = db
            .query_one("SELECT COUNT(*) c FROM receipts WHERE job_id=?", &[Val::from(jid.clone())])
            .await?;
        if rc.map(|r| r.i64_or("c", 0)).unwrap_or(0) == 0 {
            db.exec("UPDATE leases SET state='released' WHERE job_id=? AND state='active'", &[Val::from(jid.clone())]).await?;
            db.exec("UPDATE jobs SET state='deferred' WHERE id=?", &[Val::from(jid.clone())]).await?;
            db.exec(
                "INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)",
                &[Val::from(format!("job:{}", jid)), Val::from("running"), Val::from("deferred"),
                  Val::from("orphan-reap"), Val::from("no receipt 10m after submit — faulted run, replan")],
            )
            .await?;
            reaped.push(jid);
        }
    }
    let zombies = sweep_expired_claims(db).await?;
    if !zombies.is_empty() {
        out.push(json!({ "zombie_sweep": zombies }));
    }
    // REPLAN
    let mut replanned: Vec<Value> = Vec::new();
    let dq = db
        .query_all("SELECT * FROM jobs WHERE state='deferred' AND COALESCE(defer_count,0) < 8 ORDER BY created_at ASC LIMIT 2", &[])
        .await?;
    for j in dq {
        let jid = j.text_or("id", "");
        let live = db
            .query_one(
                "SELECT COUNT(*) c FROM leases l LEFT JOIN claims c ON c.lease_id=l.id
                 WHERE l.job_id=? AND l.state IN ('active','claimed') AND l.expires_at > ?
                 AND (c.claim_id IS NULL OR c.status IN ('active','completing'))",
                &[Val::from(jid.clone()), Val::from(now_iso_s.clone())],
            )
            .await?;
        if live.map(|r| r.i64_or("c", 0)).unwrap_or(0) > 0 {
            replanned.push(json!({ "job": jid, "state": "deferred",
                "note": "delegation live — awaiting claimant, not replanned" }));
            continue;
        }
        db.exec("UPDATE jobs SET defer_count=COALESCE(defer_count,0)+1, state='running' WHERE id=?", &[Val::from(jid.clone())]).await?;
        let spec: Value = serde_json::from_str(&j.text_or("spec", "{}")).unwrap_or(json!({}));
        match run_job(db, &jid, &spec, env, true).await {
            Ok(rr) => replanned.push(json!({ "job": jid,
                "state": rr.get("state").and_then(|v| v.as_str()).unwrap_or("?") })),
            Err(e) => {
                db.exec("UPDATE jobs SET state='deferred' WHERE id=?", &[Val::from(jid.clone())]).await?;
                let err_text: String = e.0.chars().take(300).collect();
                db.exec(
                    "INSERT INTO receipts (id,job_id,result,policy_version,decision,errors) VALUES (?,?,?,?,?,?)",
                    &[Val::from(uid("rcpt")), Val::from(jid.clone()), Val::from("deferred"),
                      Val::from(policy_version(db).await?),
                      Val::from(json!({ "replan": "failed", "trigger": "cron" }).to_string()),
                      Val::from(err_text.clone())],
                )
                .await?;
                replanned.push(json!({ "job": jid, "state": "deferred",
                    "error": err_text.chars().take(120).collect::<String>() }));
            }
        }
    }
    let billing = billing_check(db).await?;
    let pruned = prune_health_observations(db).await?;
    let seals_pruned = prune_seal_registry(db).await?;
    let discovery = run_discovery(db, env).await?;
    Ok(json!({ "probes": out, "providers": providers, "reaped": reaped,
               "replanned": replanned, "billing": billing, "pruned": pruned,
               "sealsPruned": seals_pruned, "discovery": discovery }))
}
