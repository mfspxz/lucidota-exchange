//! Torture: durable replay registry (Finding 3 re-attack, NOVEL vectors).
//!
//! Port of `tests/torture-seal.mjs`. v0.6.16: D1 seal_registry(seal PK,
//! first_seen); one atomic INSERT..ON CONFLICT DO NOTHING per ingest
//! (changes=0 => 409 replay); in-memory seen-set as read-free fast path;
//! REPLAY_MAX_SEEN fail-closed 503; pruneSealRegistry 500 rows/cron; 1h TTL.

mod harness;

use exchange_core::db::{Db, RowExt, Val};
use exchange_core::env::Env;
use exchange_core::routes::{handle, Request};
use exchange_core::util::{now_iso, sha256hex};
use harness::{fresh_db, test_env, SqliteDb};
use serde_json::{json, Value};
use std::collections::HashMap;

const TEST_KEY_HEX: &str = "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";

fn test_env_with_key() -> Env {
    let mut e = test_env();
    e.secrets.insert("PL_SIG_KEY".to_string(), TEST_KEY_HEX.to_string());
    e
}

fn admin_headers() -> HashMap<String, String> {
    let mut h = HashMap::new();
    h.insert("authorization".to_string(), "Bearer test-admin-key".to_string());
    h.insert("content-type".to_string(), "application/json".to_string());
    h
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

static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn mint_observe() -> Value {
    let n = NONCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let key = hex::decode(TEST_KEY_HEX).unwrap();
    let mut payload = serde_json::Map::new();
    payload.insert("kind".to_string(), Value::from("observation"));
    payload.insert("resource_id".to_string(), Value::from("r1"));
    payload.insert("ok".to_string(), Value::Bool(true));
    payload.insert("nonce".to_string(), Value::from(format!("o{}", n)));
    let map = pl::envelope::wrap_with(&key, "luci", "cA", "established", payload, "GENESIS", &now_iso())
        .expect("mint observe envelope");
    Value::Object(map)
}

async fn seed_seal_db(db: &SqliteDb) {
    db.exec(
        "INSERT INTO providers (id,name,kind,state) VALUES (?,?,?,?)",
        &[Val::from("p1"), Val::from("Test"), Val::from("test"), Val::from("UP")],
    ).await.unwrap();
    db.exec(
        "INSERT INTO resources (id,provider_id,name,kind,state,cost_class,config) VALUES (?,?,?,?,?,?,?)",
        &[Val::from("r1"), Val::from("p1"), Val::from("TestRes"), Val::from("test"),
          Val::from("AVAILABLE"), Val::from("free"), Val::from("{}")],
    ).await.unwrap();
    db.exec("INSERT INTO quotas (resource_id,limit_per_min) VALUES (?,?)",
        &[Val::from("r1"), Val::from(60i64)]).await.unwrap();
    db.exec("INSERT INTO workers (worker_id,secret_hash) VALUES (?,?)",
        &[Val::from("w1"), Val::from(sha256hex("s3cret"))]).await.unwrap();
}

async fn observe(db: &SqliteDb, env: &Env, envelope: Value) -> exchange_core::routes::Response {
    handle(db, &post("/v1/observe", json!({"resource_id": "r1", "ok": true, "envelope": envelope}), admin_headers()), env).await
}

/// S1: TTL re-admission is by design (1h window).
/// A seal older than the TTL can be re-admitted (the row was pruned).
#[tokio::test]
async fn s1_ttl_readmission_by_design() {
    let db = fresh_db().await;
    seed_seal_db(&db).await;
    let env = test_env_with_key();
    let env1 = mint_observe();
    let r1 = observe(&db, &env, env1.clone()).await;
    assert_eq!(r1.status, 200, "first observe: {:?}", r1.body);
    // Same envelope again → 409 replay.
    let r2 = observe(&db, &env, env1.clone()).await;
    assert_eq!(r2.status, 409, "replay rejected: {:?}", r2.body);
    // Simulate TTL expiry: backdate the seal row (epoch millis, matching storage).
    let seal = env1["seal"].as_str().unwrap();
    let old = chrono::Utc::now().timestamp_millis() - 2 * 3600 * 1000;
    db.exec(
        "UPDATE seal_registry SET first_seen = ? WHERE seal=?",
        &[Val::from(old), Val::from(seal)],
    ).await.unwrap();
    // Run the cron prune (public in the observe module).
    exchange_core::observe::prune_seal_registry(&db).await.unwrap();
    // Fresh isolate: clear the in-memory seal set (each WASM isolate has its
    // own statics in production; the test simulates this).
    exchange_core::routes::clear_seen_seals_for_test();
    let r3 = observe(&db, &env, env1).await;
    assert_eq!(r3.status, 200, "TTL-expired seal re-admitted by design: {:?}", r3.body);
}

/// S4: replay of a seal is rejected (409) on the same DB.
#[tokio::test]
async fn s4_replay_rejected() {
    let db = fresh_db().await;
    seed_seal_db(&db).await;
    let env = test_env_with_key();
    let env1 = mint_observe();
    let r1 = observe(&db, &env, env1.clone()).await;
    assert_eq!(r1.status, 200);
    let r2 = observe(&db, &env, env1.clone()).await;
    assert_eq!(r2.status, 409, "duplicate seal → 409");
    let r3 = observe(&db, &env, env1).await;
    assert_eq!(r3.status, 409, "triplicate seal → 409");
}

/// S2: concurrent same-seal ingest — exactly one wins, rest get 409.
#[tokio::test]
async fn s2_concurrent_same_seal_race() {
    let db = fresh_db().await;
    seed_seal_db(&db).await;
    let env = test_env_with_key();
    let env1 = mint_observe();
    // 8 concurrent ingests of the same envelope. join! polls them
    // concurrently; the atomic INSERT..ON CONFLICT ensures exactly one wins.
    let db_ref = &db;
    let env_ref = &env;
    let (s1, s2, s3, s4, s5, s6, s7, s8) = tokio::join!(
        observe(db_ref, env_ref, env1.clone()),
        observe(db_ref, env_ref, env1.clone()),
        observe(db_ref, env_ref, env1.clone()),
        observe(db_ref, env_ref, env1.clone()),
        observe(db_ref, env_ref, env1.clone()),
        observe(db_ref, env_ref, env1.clone()),
        observe(db_ref, env_ref, env1.clone()),
        observe(db_ref, env_ref, env1.clone()),
    );
    let mut ok = 0;
    let mut conflict = 0;
    for s in [s1.status, s2.status, s3.status, s4.status, s5.status, s6.status, s7.status, s8.status] {
        match s {
            200 => ok += 1,
            409 => conflict += 1,
            s => panic!("unexpected status {}", s),
        }
    }
    assert_eq!(ok, 1, "exactly one ingest wins the race");
    assert_eq!(conflict, 7, "rest get 409 replay");
}
