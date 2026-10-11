//! Torture: completeClaimed validation ordering + claim-lifecycle races.
//!
//! Port of `tests/torture-complete.mjs` (Finding 2 re-attack, NOVEL vectors).
//! Vectors: outcome type-confusion battery, concurrent double-complete,
//! sweep-interleaved double-execution (C3), sweep-vs-fresh-completion.

mod harness;

use exchange_core::db::{Db, DbError, RowExt, Val};
use exchange_core::env::Env;
use exchange_core::routes::{handle, Request};
use exchange_core::util::{now_iso, sha256hex};
use harness::{fresh_db, test_env, SqliteDb};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const TEST_KEY_HEX: &str = "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";

fn mint_envelope(claim_id: &str) -> Value {
    let key = hex::decode(TEST_KEY_HEX).unwrap();
    let mut payload = serde_json::Map::new();
    payload.insert("kind".to_string(), Value::from("completion"));
    payload.insert("worker_outcome".to_string(), Value::from("SUCCEEDED"));
    let map = pl::envelope::wrap_with(&key, "luci", claim_id, "established", payload, "GENESIS", &now_iso())
        .expect("mint envelope");
    Value::Object(map)
}

fn post(path: &str, body: Value, headers: HashMap<String, String>) -> Request {
    Request {
        method: "POST".to_string(),
        path: path.to_string(),
        query: HashMap::new(),
        headers,
        body: Some(body),
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

async fn seed_claim(db: &SqliteDb) {
    let f = future_iso();
    db.exec(
        "INSERT INTO providers (id,name,kind,state) VALUES (?,?,?,?)",
        &[Val::from("p1"), Val::from("Test"), Val::from("test"), Val::from("UP")],
    )
    .await
    .unwrap();
    db.exec(
        "INSERT INTO resources (id,provider_id,name,kind,state,cost_class,config) VALUES (?,?,?,?,?,?,?)",
        &[Val::from("r1"), Val::from("p1"), Val::from("TestRes"), Val::from("test"),
          Val::from("AVAILABLE"), Val::from("free"), Val::from("{}")],
    )
    .await
    .unwrap();
    db.exec("INSERT INTO quotas (resource_id,limit_per_min) VALUES (?,?)",
        &[Val::from("r1"), Val::from(60i64)]).await.unwrap();
    db.exec("INSERT INTO workers (worker_id,secret_hash) VALUES (?,?)",
        &[Val::from("w1"), Val::from(sha256hex("s3cret"))]).await.unwrap();
    db.exec(
        "INSERT INTO jobs (id,organelle,capability,spec,state) VALUES (?,?,?,?,?)",
        &[Val::from("jX"), Val::from("test"), Val::from("inference"),
          Val::from(json!({"capability": "inference", "input": {"model": "m1"}}).to_string()),
          Val::from("running")],
    )
    .await
    .unwrap();
    db.exec(
        "INSERT INTO leases (id,job_id,resource_id,state,expires_at) VALUES (?,?,?,?,?)",
        &[Val::from("lX"), Val::from("jX"), Val::from("r1"), Val::from("active"), Val::from(f.clone())],
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

async fn claim_status(db: &SqliteDb, id: &str) -> String {
    db.query_one("SELECT status FROM claims WHERE claim_id=?", &[Val::from(id.to_string())])
        .await
        .unwrap()
        .map(|r| r.text_or("status", ""))
        .unwrap_or_default()
}

async fn receipts_for(db: &SqliteDb, c: &str) -> i64 {
    db.query_one("SELECT COUNT(*) n FROM receipts WHERE claim_id=?", &[Val::from(c.to_string())])
        .await
        .unwrap()
        .map(|r| r.i64_or("n", 0))
        .unwrap_or(0)
}

async fn accts_for(db: &SqliteDb, c: &str) -> i64 {
    db.query_one("SELECT COUNT(*) n FROM accounting WHERE claim_id=?", &[Val::from(c.to_string())])
        .await
        .unwrap()
        .map(|r| r.i64_or("n", 0))
        .unwrap_or(0)
}

fn complete_body(claim_id: &str, outcome: Value) -> Value {
    let mut m = json!({
        "worker_id": "w1", "worker_secret": "s3cret",
        "claim_id": claim_id,
        "output": { "content": "done" },
        "envelope": mint_envelope(claim_id),
    });
    if !outcome.is_null() {
        m["worker_outcome"] = outcome;
    }
    m
}

/// C1: outcome type-confusion battery → 400, claim stays active.
#[tokio::test]
async fn c1_outcome_type_confusion() {
    let db = fresh_db().await;
    seed_claim(&db).await;
    let env = test_env_with_key();
    let cases: Vec<(&str, Value)> = vec![
        ("null", Value::Null),
        ("missing-key", Value::Null), // sentinel: omit the key
        ("array", json!(["SUCCEEDED"])),
        ("object", json!({"x": 1})),
        ("number", json!(123)),
        ("trailing-space", json!("SUCCEEDED ")),
        ("empty", json!("")),
        ("lowercase", json!("succeeded")),
        ("boolean", json!(true)),
    ];
    for (label, outcome) in &cases {
        let body = if *label == "missing-key" {
            let mut m = json!({
                "worker_id": "w1", "worker_secret": "s3cret",
                "claim_id": "cX",
                "output": { "content": "done" },
                "envelope": mint_envelope("cX"),
            });
            m.as_object_mut().unwrap().remove("worker_outcome");
            m
        } else {
            complete_body("cX", outcome.clone())
        };
        // Each case needs a FRESH envelope (replay protection).
        let mut body = body;
        body["envelope"] = mint_envelope("cX");
        let r = handle(&db, &post("/v1/complete", body, worker_headers()), &env).await;
        let st = claim_status(&db, "cX").await;
        assert_eq!(r.status, 400, "C1 {}: expected 400, got {}", label, r.status);
        assert_eq!(st, "active", "C1 {}: claim must stay active, got {}", label, st);
    }
    // C1b: claim still completable after the 400 battery.
    let r = handle(&db, &post("/v1/complete", complete_body("cX", json!("SUCCEEDED")), worker_headers()), &env).await;
    assert_eq!(r.status, 200, "C1b: expected 200, got {} {:?}", r.status, r.body);
    assert_eq!(claim_status(&db, "cX").await, "completed");
}

/// C2: concurrent double-complete → exactly 1 receipt + 1 accounting row.
#[tokio::test]
async fn c2_concurrent_double_complete() {
    let db = Arc::new(fresh_db().await);
    seed_claim(&db).await;
    let env = Arc::new(test_env_with_key());
    // Warm the resource (observation) so the claim path is realistic.
    // /v1/observe needs an envelope with a payload.resource_id binding.
    let key = hex::decode(TEST_KEY_HEX).unwrap();
    let mut payload = serde_json::Map::new();
    payload.insert("resource_id".to_string(), Value::from("r1"));
    let wenv = Value::Object(
        pl::envelope::wrap_with(&key, "luci", "warm", "established", payload, "GENESIS", &now_iso()).unwrap(),
    );
    let warm = post("/v1/observe", json!({ "resource_id": "r1", "ok": true, "envelope": wenv }), admin_headers());
    let _ = handle(db.as_ref(), &warm, &env).await;

    let db1 = db.clone();
    let db2 = db.clone();
    let e1 = env.clone();
    let e2 = env.clone();
    let (ra, rb) = tokio::join!(
        async move {
            handle(db1.as_ref(), &post("/v1/complete", complete_body("cX", json!("SUCCEEDED")), worker_headers()), &e1).await
        },
        async move {
            // Second attempt needs a fresh envelope (replay protection).
            let mut b = complete_body("cX", json!("SUCCEEDED"));
            b["envelope"] = mint_envelope("cX");
            // Sleep a tick so the first attempt wins the flip deterministically.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            handle(db2.as_ref(), &post("/v1/complete", b, worker_headers()), &e2).await
        }
    );
    let mut codes = [ra.status, rb.status];
    codes.sort();
    let rc = receipts_for(&db, "cX").await;
    let ac = accts_for(&db, "cX").await;
    assert!(
        (codes == [200, 200] || codes == [200, 409]) && rc == 1 && ac == 1,
        "C2: statuses {:?}, receipts={}, accounting={} (want 1+1)",
        codes, rc, ac
    );
    assert_eq!(claim_status(&db, "cX").await, "completed");
}

/// C3: sweep must NOT reset an in-flight completion (flip-time keyed).
#[tokio::test]
async fn c3_sweep_cannot_reset_inflight() {
    let db = fresh_db().await;
    seed_claim(&db).await;
    // Age the claimed_at past the 15min threshold (but lease still valid).
    // The OLD (buggy) code keyed the sweep on claimed_at; the fix keys on
    // completing_at.
    db.exec(
        "UPDATE claims SET claimed_at=datetime('now','-20 minutes') WHERE claim_id='cX'",
        &[],
    )
    .await
    .unwrap();
    let env = test_env_with_key();

    // Simulate an in-flight completion: status='completing' with a FRESH
    // completing_at (the flip-time key).
    db.exec(
        "UPDATE claims SET status='completing', completing_at=datetime('now') WHERE claim_id='cX'",
        &[],
    )
    .await
    .unwrap();
    // Fire the sweep.
    let sw = handle(&db, &post("/v1/admin/sweep-expired-claims", json!({}), admin_headers()), &env).await;
    let sweep_hit = sw.body["swept"]
        .as_array()
        .map(|a| a.iter().any(|s| s["claim"] == "cX"))
        .unwrap_or(false);
    let after = claim_status(&db, "cX").await;
    assert!(!sweep_hit, "C3: sweep must not touch in-flight completion");
    assert_eq!(after, "completing", "C3: claim stays completing, not reset to active");

    // And a STUCK completing (completing_at old) SHOULD be swept.
    db.exec(
        "UPDATE claims SET completing_at=datetime('now','-20 minutes') WHERE claim_id='cX'",
        &[],
    )
    .await
    .unwrap();
    let sw2 = handle(&db, &post("/v1/admin/sweep-expired-claims", json!({}), admin_headers()), &env).await;
    let swept2 = sw2.body["swept"]
        .as_array()
        .map(|a| a.iter().any(|s| s["claim"] == "cX"))
        .unwrap_or(false);
    assert!(swept2, "C3: stuck completing (old completing_at) SHOULD be swept");
}

/// C4: sweep leaves a fresh completion alone.
#[tokio::test]
async fn c4_sweep_leaves_fresh_completion() {
    let db = fresh_db().await;
    seed_claim(&db).await; // claimed_at = now (fresh)
    let env = test_env_with_key();
    // Complete it (fast, no race needed for the invariant).
    let r1 = handle(&db, &post("/v1/complete", complete_body("cX", json!("SUCCEEDED")), worker_headers()), &env).await;
    assert_eq!(r1.status, 200);
    // Sweep must not touch a completed claim.
    let sw = handle(&db, &post("/v1/admin/sweep-expired-claims", json!({}), admin_headers()), &env).await;
    let touched = sw.body["swept"]
        .as_array()
        .map(|a| a.iter().any(|s| s["claim"] == "cX"))
        .unwrap_or(false);
    let rc = receipts_for(&db, "cX").await;
    assert!(!touched, "C4: sweep must not touch completed claim");
    assert_eq!(rc, 1);
    assert_eq!(claim_status(&db, "cX").await, "completed");
}
