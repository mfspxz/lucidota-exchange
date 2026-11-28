//! v0.7.x parity tests — the flip-readiness gap closures:
//!
//! - migrate() creates the v0.7.0 tables + allow_paid fail-closed seed
//! - GET /v1/capabilities, GET/POST /v1/config, GET /v1/config/log
//! - config ENFORCEMENT in govern() (disabled key / allowlist / cost
//!   cap / allow_paid / unfunded / unlisted / capability mismatch /
//!   failover-pool pin refusal)
//! - claim-time exact-model binding (delegation requested_model from
//!   the job spec first, model_exact) + claim resume:true (v0.7.2)
//! - MODEL_MISMATCH taxonomy (v0.7.3) + exact-model verification on
//!   complete + model_capabilities execution bookkeeping
//! - verify gauntlet inference-first ordering (v0.7.1)
//! - version stamp parity (0.7.3) + health counts

mod harness;

use exchange_core::db::{Db, RowExt, Val};
use exchange_core::env::Env;
use exchange_core::routes::{handle, Request, Response};
use exchange_core::util::{now_iso, sha256hex};
use harness::{fresh_db, test_env, SqliteDb};
use serde_json::{json, Value};
use std::collections::HashMap;

const TEST_KEY_HEX: &str = "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";

fn req(method: &str, path: &str, body: Option<Value>, headers: HashMap<String, String>) -> Request {
    Request {
        method: method.to_string(),
        path: path.to_string(),
        query: HashMap::new(),
        headers,
        body,
    }
}

fn req_q(method: &str, path: &str, query: Vec<(&str, &str)>, headers: HashMap<String, String>) -> Request {
    Request {
        method: method.to_string(),
        path: path.to_string(),
        query: query.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        headers,
        body: None,
    }
}

fn admin_headers() -> HashMap<String, String> {
    let mut h = HashMap::new();
    h.insert("authorization".to_string(), "Bearer test-admin-key".to_string());
    h.insert("content-type".to_string(), "application/json".to_string());
    h
}

fn worker_headers() -> HashMap<String, String> {
    let mut h = HashMap::new();
    h.insert("x-worker-id".to_string(), "w1".to_string());
    h.insert("x-worker-secret".to_string(), "s3cret".to_string());
    h.insert("content-type".to_string(), "application/json".to_string());
    h
}

fn test_env_with_key() -> Env {
    let mut e = test_env();
    e.secrets.insert("PL_SIG_KEY".to_string(), TEST_KEY_HEX.to_string());
    e
}

fn future_iso() -> String {
    (chrono::Utc::now() + chrono::Duration::hours(1))
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

fn mint_envelope(claim_id: &str) -> Value {
    let key = hex::decode(TEST_KEY_HEX).unwrap();
    let mut payload = serde_json::Map::new();
    payload.insert("kind".to_string(), Value::from("completion"));
    payload.insert("worker_outcome".to_string(), Value::from("SUCCEEDED"));
    let map = pl::envelope::wrap_with(&key, "luci", claim_id, "established", payload, "GENESIS", &now_iso())
        .expect("mint envelope");
    Value::Object(map)
}

async fn insert_provider(db: &SqliteDb) {
    db.exec(
        "INSERT INTO providers (id,name,kind,state) VALUES (?,?,?,?)",
        &[Val::from("p1"), Val::from("Test"), Val::from("test"), Val::from("UP")],
    )
    .await
    .unwrap();
    // Production DBs already ran the one-time v0.3 evidence backfill;
    // without this marker, migrate() (run by handle()) would backfill
    // on first request and rewrite the evidence_state these tests seed.
    db.exec(
        "INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)",
        &[
            Val::from("test-harness"),
            Val::from("UNKNOWN"),
            Val::from("UNKNOWN"),
            Val::from("backfill-v0.3"),
            Val::from("test marker: backfill already applied"),
        ],
    )
    .await
    .unwrap();
}

async fn insert_resource(db: &SqliteDb, id: &str, config: Value, evidence: &str) {
    db.exec(
        "INSERT INTO resources (id,provider_id,name,kind,state,cost_class,sovereignty,config,evidence_state,evidence_source) VALUES (?,?,?,?,?,?,?,?,?,?)",
        &[
            Val::from(id),
            Val::from("p1"),
            Val::from(format!("Res-{}", id)),
            Val::from("ai-lane"),
            Val::from("AVAILABLE"),
            Val::from("free"),
            Val::from("external"),
            Val::from(config.to_string()),
            Val::from(evidence),
            Val::from("test-seed"),
        ],
    )
    .await
    .unwrap();
}

async fn insert_cap(db: &SqliteDb, rid: &str, cap: &str) {
    db.exec(
        "INSERT INTO resource_capabilities (resource_id,capability,detail) VALUES (?,?,?)",
        &[Val::from(rid), Val::from(cap), Val::from("{}")],
    )
    .await
    .unwrap();
}

async fn insert_model_cap(
    db: &SqliteDb,
    rid: &str,
    model: &str,
    cap: &str,
    cost: &str,
    last_executed_at: Option<&str>,
) {
    db.exec(
        "INSERT INTO model_capabilities (resource_id,model,capability,cost,source,listed_at,last_executed_at) VALUES (?,?,?,?,?,?,?)",
        &[
            Val::from(rid),
            Val::from(model),
            Val::from(cap),
            Val::from(cost),
            Val::from("test"),
            Val::from(now_iso()),
            last_executed_at.map(Val::from).unwrap_or(Val::Null),
        ],
    )
    .await
    .unwrap();
}

async fn insert_worker(db: &SqliteDb) {
    db.exec(
        "INSERT INTO workers (worker_id,secret_hash) VALUES (?,?)",
        &[Val::from("w1"), Val::from(sha256hex("s3cret"))],
    )
    .await
    .unwrap();
}

async fn submit_job(db: &SqliteDb, env: &Env, spec: Value) -> Response {
    handle(db, &req("POST", "/v1/job", Some(spec), admin_headers()), env).await
}

fn rejection(resp: &Response, gate: &str) -> Option<String> {
    resp.body["rejected"]
        .as_array()?
        .iter()
        .find(|r| r["gate"] == gate)
        .map(|r| r["reason"].as_str().unwrap_or("").to_string())
}

// ---------------------------------------------------------------------------
// Migration + version + health
// ---------------------------------------------------------------------------

#[tokio::test]
async fn migrate_creates_v07_tables_and_allow_paid_seed() {
    let db = fresh_db().await;
    for t in ["model_capabilities", "key_config", "config_log", "exchange_config"] {
        let r = db
            .query_one(
                "SELECT name FROM sqlite_master WHERE type='table' AND name=?",
                &[Val::from(t)],
            )
            .await
            .unwrap();
        assert!(r.is_some(), "table {} must exist after migrate", t);
    }
    let ap = db
        .query_one("SELECT value FROM exchange_config WHERE key='allow_paid'", &[])
        .await
        .unwrap()
        .expect("allow_paid seeded");
    assert_eq!(ap.text_or("value", ""), "0", "allow_paid fail-closed default");
}

#[tokio::test]
async fn version_stamp_and_health_counts() {
    let db = fresh_db().await;
    let env = test_env();
    insert_provider(&db).await;
    insert_resource(&db, "r1", json!({ "key_id": "k1" }), "VERIFIED").await;
    insert_model_cap(&db, "r1", "m1", "inference", "free", None).await;
    db.exec(
        "INSERT INTO key_config (key_id,enabled) VALUES (?,?)",
        &[Val::from("k1"), Val::from(1i64)],
    )
    .await
    .unwrap();
    let root = handle(&db, &req("GET", "/", None, HashMap::new()), &env).await;
    assert_eq!(root.body["version"], "0.7.3");
    let health = handle(&db, &req("GET", "/v1/health", None, HashMap::new()), &env).await;
    assert_eq!(health.body["version"], "0.7.3");
    assert_eq!(health.body["resources"], 1);
    assert_eq!(health.body["capabilities"], 1);
    assert_eq!(health.body["keys"], 1);
}

// ---------------------------------------------------------------------------
// /v1/capabilities
// ---------------------------------------------------------------------------

async fn seed_capabilities(db: &SqliteDb) {
    insert_provider(db).await;
    insert_resource(db, "r1", json!({ "key_id": "k1" }), "VERIFIED").await;
    insert_resource(db, "r2", json!({}), "UNKNOWN").await;
    insert_model_cap(db, "r1", "m1", "inference", "free", Some("2026-11-20T00:00:00.000Z")).await;
    insert_model_cap(db, "r1", "m2", "embedding", "paid", None).await;
    insert_model_cap(db, "r2", "m3", "inference", "unfunded", None).await;
    db.exec(
        "INSERT INTO key_config (key_id,enabled) VALUES (?,?)",
        &[Val::from("k1"), Val::from(1i64)],
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn capabilities_listing_counts_and_filters() {
    let db = fresh_db().await;
    let env = test_env();
    seed_capabilities(&db).await;

    let r = handle(&db, &req("GET", "/v1/capabilities", None, admin_headers()), &env).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body["total"], 3);
    assert_eq!(r.body["proven"], 1, "only m1 has last_executed_at");
    assert_eq!(r.body["usable"], 2, "m1 (free) + m2 (paid) usable; m3 unfunded + unverified");
    assert_eq!(r.body["by_capability"]["inference"], 2);
    assert_eq!(r.body["by_capability"]["embedding"], 1);
    assert_eq!(r.body["by_cost"]["free"], 1);
    assert_eq!(r.body["by_cost"]["paid"], 1);
    assert_eq!(r.body["by_cost"]["unfunded"], 1);
    let caps = r.body["capabilities"].as_array().unwrap();
    let m1 = caps.iter().find(|c| c["model"] == "m1").unwrap();
    assert_eq!(m1["key_id"], "k1");
    assert_eq!(m1["key_enabled"], true);
    assert_eq!(m1["proven"], true);
    assert_eq!(m1["usable"], true);
    let m3 = caps.iter().find(|c| c["model"] == "m3").unwrap();
    assert_eq!(m3["key_id"], Value::Null);
    assert_eq!(m3["usable"], false);

    let r = handle(
        &db,
        &req_q("GET", "/v1/capabilities", vec![("usable", "1")], admin_headers()),
        &env,
    )
    .await;
    assert_eq!(r.body["total"], 2);
    let r = handle(
        &db,
        &req_q("GET", "/v1/capabilities", vec![("capability", "embedding")], admin_headers()),
        &env,
    )
    .await;
    assert_eq!(r.body["total"], 1);
    let r = handle(
        &db,
        &req_q("GET", "/v1/capabilities", vec![("key", "k1")], admin_headers()),
        &env,
    )
    .await;
    assert_eq!(r.body["total"], 2);
    let r = handle(
        &db,
        &req_q("GET", "/v1/capabilities", vec![("cost", "unfunded")], admin_headers()),
        &env,
    )
    .await;
    assert_eq!(r.body["total"], 1);

    // Disabled key → nothing on that key is usable.
    db.exec("UPDATE key_config SET enabled=0 WHERE key_id='k1'", &[]).await.unwrap();
    let r = handle(&db, &req("GET", "/v1/capabilities", None, admin_headers()), &env).await;
    assert_eq!(r.body["usable"], 0);
    let caps = r.body["capabilities"].as_array().unwrap();
    assert_eq!(caps.iter().find(|c| c["model"] == "m1").unwrap()["key_enabled"], false);

    // Auth gate: no admin key → 401.
    let r = handle(&db, &req("GET", "/v1/capabilities", None, HashMap::new()), &env).await;
    assert_eq!(r.status, 401);
}

// ---------------------------------------------------------------------------
// /v1/config GET/POST + /v1/config/log
// ---------------------------------------------------------------------------

#[tokio::test]
async fn config_global_roundtrip_and_validation() {
    let db = fresh_db().await;
    let env = test_env();

    let r = handle(
        &db,
        &req("POST", "/v1/config", Some(json!({ "scope": "global", "field": "allow_paid", "value": 1, "reason": "test" })), admin_headers()),
        &env,
    )
    .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["changed"], true);
    assert_eq!(r.body["from"], "0");
    assert_eq!(r.body["to"], "1");
    assert!(r.body["log_id"].is_number(), "log_id must be the config_log row id");
    assert!(r.body["note"].as_str().unwrap().contains("PAID EXECUTION ENABLED"));

    let r = handle(&db, &req("GET", "/v1/config", None, admin_headers()), &env).await;
    assert_eq!(r.body["global"]["allow_paid"], "1");
    assert_eq!(r.body["recent_changes"][0]["field"], "allow_paid");
    assert_eq!(r.body["recent_changes"][0]["scope"], "global");

    let r = handle(&db, &req("GET", "/v1/config/log", None, admin_headers()), &env).await;
    assert_eq!(r.body["log"].as_array().unwrap().len(), 1);

    // Validation battery.
    for (body, want) in [
        (json!({ "scope": "global", "field": "bogus", "value": 1 }), "global field must be allow_paid"),
        (json!({ "scope": "global", "field": "allow_paid", "value": 2 }), "allow_paid must be 0 or 1"),
        (json!({ "scope": "nope", "field": "allow_paid", "value": 1 }), "scope must be 'key' or 'global'"),
    ] {
        let r = handle(&db, &req("POST", "/v1/config", Some(body), admin_headers()), &env).await;
        assert_eq!(r.status, 400);
        assert_eq!(r.body["error"], want);
    }
    // evidence_log carries the change too.
    let n = db
        .query_one(
            "SELECT COUNT(*) c FROM evidence_log WHERE trigger='config-change'",
            &[],
        )
        .await
        .unwrap()
        .map(|r| r.i64_or("c", 0))
        .unwrap_or(0);
    assert_eq!(n, 1);
}

#[tokio::test]
async fn config_key_roundtrip_and_validation() {
    let db = fresh_db().await;
    let env = test_env();
    insert_provider(&db).await;
    insert_resource(&db, "r1", json!({ "key_id": "k1" }), "VERIFIED").await;
    insert_model_cap(&db, "r1", "m1", "inference", "free", None).await;

    let post = post_cfg;

    // enabled=0
    let r = post(&db, &env, json!({ "scope": "key", "key_id": "k1", "field": "enabled", "value": 0 })).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["from"], "1");
    assert_eq!(r.body["to"], "0");
    let r = handle(&db, &req("GET", "/v1/config", None, admin_headers()), &env).await;
    assert_eq!(r.body["keys"][0]["enabled"], false);
    assert_eq!(r.body["keys"][0]["key_id"], "k1");

    // allowlist with a listed model ok; unlisted refused.
    let r = post(&db, &env, json!({ "scope": "key", "key_id": "k1", "field": "allowlist", "value": ["m1"] })).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    let r = handle(&db, &req("GET", "/v1/config", None, admin_headers()), &env).await;
    assert_eq!(r.body["keys"][0]["allowlist"], json!(["m1"]));
    let r = post(&db, &env, json!({ "scope": "key", "key_id": "k1", "field": "allowlist", "value": ["ghost"] })).await;
    assert_eq!(r.status, 400);
    assert!(r.body["error"].as_str().unwrap().contains("not listed for any resource: ghost"));

    // allowlist_clear
    let r = post(&db, &env, json!({ "scope": "key", "key_id": "k1", "field": "allowlist_clear" })).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body["field"], "allowlist");
    assert_eq!(r.body["to"], "null");
    let r = handle(&db, &req("GET", "/v1/config", None, admin_headers()), &env).await;
    assert_eq!(r.body["keys"][0]["allowlist"], Value::Null);

    // cost_cap / failover_order
    let r = post(&db, &env, json!({ "scope": "key", "key_id": "k1", "field": "cost_cap", "value": 2.5 })).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body["to"], "2.5");
    let r = post(&db, &env, json!({ "scope": "key", "key_id": "k1", "field": "cost_cap", "value": -1 })).await;
    assert_eq!(r.status, 400);
    let r = post(&db, &env, json!({ "scope": "key", "key_id": "k1", "field": "failover_order", "value": 3 })).await;
    assert_eq!(r.status, 200);
    let r = post(&db, &env, json!({ "scope": "key", "key_id": "k1", "field": "failover_order", "value": -1 })).await;
    assert_eq!(r.status, 400);
    let r = post(&db, &env, json!({ "scope": "key", "key_id": "k1", "field": "bogus", "value": 1 })).await;
    assert_eq!(r.status, 400);
    assert!(r.body["error"].as_str().unwrap().contains("field must be enabled"));

    // unknown key + missing key_id
    let r = post(&db, &env, json!({ "scope": "key", "key_id": "nope", "field": "enabled", "value": 0 })).await;
    assert_eq!(r.status, 400);
    assert!(r.body["error"].as_str().unwrap().contains("unknown key_id nope"));
    let r = post(&db, &env, json!({ "scope": "key", "field": "enabled", "value": 0 })).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.body["error"], "key_id required");

    // config/log filtered by key_id
    let r = handle(
        &db,
        &req_q("GET", "/v1/config/log", vec![("key_id", "k1")], admin_headers()),
        &env,
    )
    .await;
    assert!(r.body["log"].as_array().unwrap().len() >= 4);
    let r = handle(
        &db,
        &req_q("GET", "/v1/config/log", vec![("key_id", "other")], admin_headers()),
        &env,
    )
    .await;
    assert_eq!(r.body["log"].as_array().unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// Config ENFORCEMENT in govern() — the operational landmine of the flip:
// settings operators wrote to D1 must refuse service in Rust too.
// ---------------------------------------------------------------------------

/// A claimed-adapter resource "nim1" bound to key k-nim, default model
/// m-free, evidence VERIFIED — passes every pre-v0.7 gate.
async fn seed_governable(db: &SqliteDb, config_extra: Value) {
    insert_provider(db).await;
    let mut cfg = json!({ "adapter": "claimed", "key_id": "k-nim", "model": "m-free" });
    if let (Some(dst), Some(src)) = (cfg.as_object_mut(), config_extra.as_object()) {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }
    insert_resource(db, "nim1", cfg, "VERIFIED").await;
    insert_cap(db, "nim1", "inference").await;
    insert_model_cap(db, "nim1", "m-free", "inference", "free", None).await;
}

fn inference_job(model: Option<&str>) -> Value {
    let input = match model {
        Some(m) => json!({ "model": m }),
        None => json!({}),
    };
    json!({ "capability": "inference", "input": input, "data_class": "public", "max_cost": 0 })
}

#[tokio::test]
async fn govern_baseline_selected_with_model() {
    let db = fresh_db().await;
    let env = test_env();
    seed_governable(&db, json!({})).await;
    let r = submit_job(&db, &env, inference_job(None)).await;
    assert_eq!(r.body["state"], "deferred", "claimed resource delegates: {:?}", r.body);
    assert!(rejection(&r, "MODEL").is_none(), "no MODEL rejection expected: {:?}", r.body["rejected"]);
    assert!(rejection(&r, "CONFIG").is_none());
}

#[tokio::test]
async fn govern_disabled_key_refuses() {
    let db = fresh_db().await;
    let env = test_env();
    seed_governable(&db, json!({})).await;
    db.exec(
        "INSERT INTO key_config (key_id,enabled) VALUES (?,?)",
        &[Val::from("k-nim"), Val::from(0i64)],
    )
    .await
    .unwrap();
    let r = submit_job(&db, &env, inference_job(None)).await;
    assert_eq!(r.body["state"], "blocked");
    let reason = rejection(&r, "CONFIG").expect("CONFIG rejection");
    assert!(reason.contains("key k-nim disabled by config"), "{}", reason);
}

#[tokio::test]
async fn govern_allowlist_refuses() {
    let db = fresh_db().await;
    let env = test_env();
    seed_governable(&db, json!({})).await;
    db.exec(
        "INSERT INTO key_config (key_id,enabled,allowlist) VALUES (?,?,?)",
        &[Val::from("k-nim"), Val::from(1i64), Val::from("[\"someone-else\"]")],
    )
    .await
    .unwrap();
    let r = submit_job(&db, &env, inference_job(None)).await;
    assert_eq!(r.body["state"], "blocked");
    let reason = rejection(&r, "CONFIG").expect("CONFIG rejection");
    assert!(reason.contains("model m-free not in key k-nim allowlist"), "{}", reason);
}

#[tokio::test]
async fn govern_unlisted_model_refuses() {
    let db = fresh_db().await;
    let env = test_env();
    seed_governable(&db, json!({})).await;
    let r = submit_job(&db, &env, inference_job(Some("m-ghost"))).await;
    assert_eq!(r.body["state"], "blocked");
    let reason = rejection(&r, "MODEL").expect("MODEL rejection");
    assert!(reason.contains("model m-ghost not listed for nim1"), "{}", reason);
}

#[tokio::test]
async fn govern_model_capability_mismatch_refuses() {
    let db = fresh_db().await;
    let env = test_env();
    seed_governable(&db, json!({})).await;
    db.exec(
        "UPDATE model_capabilities SET capability='embedding' WHERE resource_id='nim1' AND model='m-free'",
        &[],
    )
    .await
    .unwrap();
    let r = submit_job(&db, &env, inference_job(None)).await;
    assert_eq!(r.body["state"], "blocked");
    let reason = rejection(&r, "MODEL").expect("MODEL rejection");
    assert!(reason.contains("model m-free serves embedding, job needs inference"), "{}", reason);
}

#[tokio::test]
async fn govern_unfunded_refuses() {
    let db = fresh_db().await;
    let env = test_env();
    seed_governable(&db, json!({})).await;
    db.exec(
        "UPDATE model_capabilities SET cost='unfunded' WHERE resource_id='nim1' AND model='m-free'",
        &[],
    )
    .await
    .unwrap();
    let r = submit_job(&db, &env, inference_job(None)).await;
    assert_eq!(r.body["state"], "blocked");
    let reason = rejection(&r, "COST").expect("COST rejection");
    assert!(reason.contains("unfunded (0 credits)"), "{}", reason);
}

#[tokio::test]
async fn govern_paid_model_kill_switch_and_cost_cap() {
    let db = fresh_db().await;
    let env = test_env();
    seed_governable(&db, json!({})).await;
    db.exec(
        "UPDATE model_capabilities SET cost='paid' WHERE resource_id='nim1' AND model='m-free'",
        &[],
    )
    .await
    .unwrap();

    // allow_paid=0 (seeded default) → COST refusal.
    let r = submit_job(&db, &env, inference_job(None)).await;
    assert_eq!(r.body["state"], "blocked");
    let reason = rejection(&r, "COST").expect("COST rejection");
    assert!(reason.contains("is paid and global allow_paid=0"), "{}", reason);

    // Flip the knob through the real route; now the job needs spend authority.
    let r = handle(
        &db,
        &req("POST", "/v1/config", Some(json!({ "scope": "global", "field": "allow_paid", "value": 1 })), admin_headers()),
        &env,
    )
    .await;
    assert_eq!(r.status, 200);
    let r = submit_job(&db, &env, inference_job(None)).await;
    let reason = rejection(&r, "COST").expect("COST rejection");
    assert!(reason.contains("is paid; job has no spend_authority"), "{}", reason);

    // cost_cap below the job's spend authority → CONFIG refusal.
    db.exec(
        "INSERT INTO key_config (key_id,enabled,cost_cap) VALUES (?,?,?)",
        &[Val::from("k-nim"), Val::from(1i64), Val::from(2.0f64)],
    )
    .await
    .unwrap();
    let mut spec = inference_job(None);
    spec["spend_authority"] = json!(5);
    spec["max_cost"] = json!(5);
    let r = submit_job(&db, &env, spec).await;
    assert_eq!(r.body["state"], "blocked");
    let reason = rejection(&r, "CONFIG").expect("CONFIG rejection");
    assert!(reason.contains("spend_authority 5 exceeds key k-nim cost_cap 2"), "{}", reason);

    // Under the cap → routes (delegated).
    let mut spec = inference_job(None);
    spec["spend_authority"] = json!(1);
    spec["max_cost"] = json!(1);
    let r = submit_job(&db, &env, spec).await;
    assert_eq!(r.body["state"], "deferred", "{:?}", r.body);
}

#[tokio::test]
async fn govern_failover_pool_refuses_pin() {
    let db = fresh_db().await;
    let env = test_env();
    seed_governable(&db, json!({ "model_exact": false })).await;
    let r = submit_job(&db, &env, inference_job(Some("m-free"))).await;
    assert_eq!(r.body["state"], "blocked");
    let reason = rejection(&r, "MODEL").expect("MODEL rejection");
    assert!(reason.contains("failover pool cannot serve a pinned model"), "{}", reason);
    // Unpinned jobs still route to the pool.
    let r = submit_job(&db, &env, inference_job(None)).await;
    assert_eq!(r.body["state"], "deferred", "{:?}", r.body);
}

// ---------------------------------------------------------------------------
// Claim-time binding + resume (v0.7.0 / v0.7.2)
// ---------------------------------------------------------------------------

async fn seed_claimable(db: &SqliteDb, config: Value, spec: Value) {
    insert_provider(db).await;
    insert_resource(db, "r1", config, "VERIFIED").await;
    insert_worker(db).await;
    db.exec(
        "INSERT INTO jobs (id,organelle,capability,spec,state) VALUES (?,?,?,?,?)",
        &[
            Val::from("j1"),
            Val::from("test"),
            Val::from("inference"),
            Val::from(spec.to_string()),
            Val::from("deferred"),
        ],
    )
    .await
    .unwrap();
    db.exec(
        "INSERT INTO leases (id,job_id,resource_id,state,expires_at) VALUES (?,?,?,?,?)",
        &[
            Val::from("l1"),
            Val::from("j1"),
            Val::from("r1"),
            Val::from("active"),
            Val::from(future_iso()),
        ],
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn claim_binds_job_model_and_resume_returns_same_claim() {
    let db = fresh_db().await;
    let env = test_env();
    seed_claimable(
        &db,
        json!({ "model": "dflt", "credential_ref": "ref1" }),
        json!({ "capability": "inference", "input": { "model": "pin-1" } }),
    )
    .await;

    let r = handle(
        &db,
        &req("POST", "/v1/claim", Some(json!({ "resource_id": "r1" })), worker_headers()),
        &env,
    )
    .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["claimed"], true);
    // The JOB's pinned model wins over the resource default.
    assert_eq!(r.body["delegation"]["requested_model"], "pin-1");
    assert_eq!(r.body["delegation"]["model_exact"], true);
    assert_eq!(r.body["delegation"]["credential_ref"], "ref1");
    let claim_id = r.body["delegation"]["claim_id"].as_str().unwrap().to_string();
    let attempt_id = r.body["attempt_id"].as_str().unwrap().to_string();

    // resume:true returns the SAME live claim instead of a new lease.
    let r = handle(
        &db,
        &req("POST", "/v1/claim", Some(json!({ "resource_id": "r1", "resume": true })), worker_headers()),
        &env,
    )
    .await;
    assert_eq!(r.body["claimed"], true);
    assert_eq!(r.body["resumed"], true);
    assert_eq!(r.body["delegation"]["claim_id"], claim_id);
    assert_eq!(r.body["attempt_id"], attempt_id);
    assert_eq!(r.body["delegation"]["requested_model"], "pin-1");

    // A different worker resuming finds no claim of its own and no
    // unclaimed lease → claimed:false (no orphan, no theft).
    db.exec(
        "INSERT INTO workers (worker_id,secret_hash) VALUES (?,?)",
        &[Val::from("w2"), Val::from(sha256hex("pw2"))],
    )
    .await
    .unwrap();
    let mut h2 = HashMap::new();
    h2.insert("x-worker-id".to_string(), "w2".to_string());
    h2.insert("x-worker-secret".to_string(), "pw2".to_string());
    let r = handle(
        &db,
        &req("POST", "/v1/claim", Some(json!({ "resource_id": "r1", "resume": true })), h2),
        &env,
    )
    .await;
    assert_eq!(r.body["claimed"], false);

    // resume with NO live claim falls through to a fresh claim.
    let db2 = fresh_db().await;
    seed_claimable(
        &db2,
        json!({ "model": "dflt" }),
        json!({ "capability": "inference", "input": {} }),
    )
    .await;
    let r = handle(
        &db2,
        &req("POST", "/v1/claim", Some(json!({ "resource_id": "r1", "resume": true })), worker_headers()),
        &env,
    )
    .await;
    assert_eq!(r.body["claimed"], true);
    assert!(r.body.get("resumed").is_none());
    // Unpinned job → resource default model.
    assert_eq!(r.body["delegation"]["requested_model"], "dflt");
}

#[tokio::test]
async fn claim_delegation_model_exact_false_pool() {
    let db = fresh_db().await;
    let env = test_env();
    seed_claimable(
        &db,
        json!({ "model": "dflt", "model_exact": false }),
        json!({ "capability": "inference", "input": {} }),
    )
    .await;
    let r = handle(
        &db,
        &req("POST", "/v1/claim", Some(json!({ "resource_id": "r1" })), worker_headers()),
        &env,
    )
    .await;
    assert_eq!(r.body["claimed"], true);
    assert_eq!(r.body["delegation"]["model_exact"], false);
    assert_eq!(r.body["delegation"]["requested_model"], "dflt");
}

// ---------------------------------------------------------------------------
// Complete: exact-model verification + bookkeeping + taxonomy (v0.7.0/v0.7.3)
// ---------------------------------------------------------------------------

async fn seed_completion(db: &SqliteDb, config: Value, spec_input: Value) {
    insert_provider(db).await;
    insert_resource(db, "r1", config, "VERIFIED").await;
    insert_worker(db).await;
    db.exec("INSERT INTO quotas (resource_id,limit_per_min) VALUES (?,?)",
        &[Val::from("r1"), Val::from(60i64)]).await.unwrap();
    insert_model_cap(db, "r1", "m1", "inference", "free", None).await;
    let f = future_iso();
    db.exec(
        "INSERT INTO jobs (id,organelle,capability,spec,state) VALUES (?,?,?,?,?)",
        &[
            Val::from("jX"),
            Val::from("test"),
            Val::from("inference"),
            Val::from(json!({ "capability": "inference", "input": spec_input }).to_string()),
            Val::from("running"),
        ],
    )
    .await
    .unwrap();
    db.exec(
        "INSERT INTO leases (id,job_id,resource_id,state,expires_at) VALUES (?,?,?,?,?)",
        &[Val::from("lX"), Val::from("jX"), Val::from("r1"), Val::from("claimed"), Val::from(f.clone())],
    )
    .await
    .unwrap();
    db.exec(
        "INSERT INTO executions (id,job_id,resource_id,claim_id,request_hash) VALUES (?,?,?,?,?)",
        &[Val::from("eX"), Val::from("jX"), Val::from("r1"), Val::from("cX"), Val::from("HX")],
    )
    .await
    .unwrap();
    db.exec(
        "INSERT INTO claims (claim_id,job_id,worker_id,lease_id,resource_id,status,lease_expiry,policy_snapshot) VALUES (?,?,?,?,?,?,?,?)",
        &[Val::from("cX"), Val::from("jX"), Val::from("w1"), Val::from("lX"), Val::from("r1"),
          Val::from("active"), Val::from(f), Val::from("{}")],
    )
    .await
    .unwrap();
}

async fn complete(db: &SqliteDb, env: &Env, extra: Value) -> Response {
    let mut body = json!({
        "worker_id": "w1", "worker_secret": "s3cret",
        "claim_id": "cX",
        "envelope": mint_envelope("cX"),
    });
    if let (Some(dst), Some(src)) = (body.as_object_mut(), extra.as_object()) {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }
    handle(db, &req("POST", "/v1/complete", Some(body), worker_headers()), env).await
}

async fn post_cfg(db: &SqliteDb, env: &Env, body: Value) -> Response {
    handle(db, &req("POST", "/v1/config", Some(body), admin_headers()), env).await
}

async fn model_cap_row(db: &SqliteDb, model: &str) -> (Option<String>, Option<String>) {
    let r = db
        .query_one(
            "SELECT last_executed_at, last_error FROM model_capabilities WHERE resource_id='r1' AND model=?",
            &[Val::from(model)],
        )
        .await
        .unwrap()
        .expect("model_capabilities row");
    (r.text("last_executed_at"), r.text("last_error"))
}

#[tokio::test]
async fn complete_success_stamps_model_capabilities() {
    let db = fresh_db().await;
    let env = test_env_with_key();
    seed_completion(&db, json!({ "model": "m1" }), json!({ "model": "m1" })).await;
    let r = complete(&db, &env, json!({
        "worker_outcome": "SUCCEEDED",
        "output": { "content": "done" },
        "model_actual": "m1",
    }))
    .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["state"], "done");
    let (executed_at, last_error) = model_cap_row(&db, "m1").await;
    assert!(executed_at.is_some(), "last_executed_at must be stamped");
    assert!(last_error.is_none(), "last_error cleared on success");
}

#[tokio::test]
async fn complete_model_mismatch_is_verification_failure() {
    let db = fresh_db().await;
    let env = test_env_with_key();
    seed_completion(&db, json!({ "model": "m1" }), json!({ "model": "m1" })).await;
    let r = complete(&db, &env, json!({
        "worker_outcome": "SUCCEEDED",
        "output": { "content": "done" },
        "model_actual": "m2-substitute",
    }))
    .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["state"], "failed", "substitution must fail the job, not defer it");
    assert_eq!(r.body["taxonomy"], "VERIFICATION_FAILURE");
    let ex = db
        .query_one("SELECT error_class, failure_taxonomy, model_match FROM executions WHERE id='eX'", &[])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ex.text_or("error_class", ""), "MODEL_MISMATCH");
    assert_eq!(ex.text_or("failure_taxonomy", ""), "VERIFICATION_FAILURE");
    assert_eq!(ex.i64_or("model_match", -1), 0);
    let (_, last_error) = model_cap_row(&db, "m1").await;
    let le = last_error.expect("last_error recorded on the requested model");
    assert!(le.starts_with("model-mismatch: requested m1, executed m2-substitute"), "{}", le);
}

#[tokio::test]
async fn complete_mismatch_exempt_for_failover_pool() {
    let db = fresh_db().await;
    let env = test_env_with_key();
    seed_completion(&db, json!({ "model": "m1", "model_exact": false }), json!({})).await;
    let r = complete(&db, &env, json!({
        "worker_outcome": "SUCCEEDED",
        "output": { "content": "done" },
        "model_actual": "whatever-the-pool-picked",
    }))
    .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["state"], "done");
}

#[tokio::test]
async fn complete_not_found_is_permanent_and_demotes_default() {
    let db = fresh_db().await;
    let env = test_env_with_key();
    seed_completion(&db, json!({ "model": "m1" }), json!({ "model": "m1" })).await;
    let r = complete(&db, &env, json!({
        "worker_outcome": "FAILED_PERMANENT",
        "error_class": "NOT_FOUND",
        "error": "404 model not found",
    }))
    .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["state"], "failed", "PERMANENT_FAILURE must not defer (v0.7.3)");
    assert_eq!(r.body["taxonomy"], "PERMANENT_FAILURE");
    let (_, last_error) = model_cap_row(&db, "m1").await;
    assert_eq!(
        last_error.as_deref(),
        Some("model unavailable at provider (404/410) — claimant refused without executing")
    );
    // The dead model was the resource default → evidence demoted.
    let ev = db
        .query_one("SELECT evidence_state FROM resources WHERE id='r1'", &[])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ev.text_or("evidence_state", ""), "UNKNOWN");
    // JS parity: the bookkeeping insert AND setEvidence's own log row
    // both carry trigger='model-unavailable' → 2 rows.
    let n = db
        .query_one(
            "SELECT COUNT(*) c FROM evidence_log WHERE resource_id='r1' AND trigger='model-unavailable'",
            &[],
        )
        .await
        .unwrap()
        .map(|r| r.i64_or("c", 0))
        .unwrap_or(0);
    assert_eq!(n, 2);
}

#[test]
fn taxonomy_v073_classes() {
    use exchange_core::policy::taxonomy_of;
    assert_eq!(taxonomy_of("NOT_FOUND"), "PERMANENT_FAILURE");
    assert_eq!(taxonomy_of("BAD_REQUEST"), "PERMANENT_FAILURE");
    assert_eq!(taxonomy_of("MODEL_MISMATCH"), "VERIFICATION_FAILURE");
    assert_eq!(taxonomy_of("RATE_LIMITED"), "QUOTA_FAILURE");
    assert_eq!(taxonomy_of("TIMEOUT"), "RETRYABLE_FAILURE");
}

// ---------------------------------------------------------------------------
// Verify gauntlet: inference-first probe capability (v0.7.1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn verify_gauntlet_prefers_inference() {
    let db = fresh_db().await;
    let env = test_env();
    insert_provider(&db).await;
    insert_resource(&db, "rv", json!({ "adapter": "claimed" }), "UNKNOWN").await;
    // Embedding inserted FIRST — the v0.7.0 unordered caps[0] bug took it.
    insert_cap(&db, "rv", "embedding").await;
    insert_cap(&db, "rv", "inference").await;
    let r = handle(
        &db,
        &req("POST", "/v1/resources/rv/verify", Some(json!({})), admin_headers()),
        &env,
    )
    .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["delegated"], true);
    let job_id = r.body["job_id"].as_str().unwrap().to_string();
    let job = db
        .query_one("SELECT capability FROM jobs WHERE id=?", &[Val::from(job_id)])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.text_or("capability", ""), "inference");
}
