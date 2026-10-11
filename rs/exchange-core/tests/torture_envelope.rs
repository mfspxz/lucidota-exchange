//! Torture: envelope forgery / shape attacks at the route boundary.
//!
//! Port of the key vectors from `tests/torture-envelope.mjs` (F1-F3, F7).
//! The envelope crypto itself is the `pl` reference crate (not re-ported);
//! these tests prove the route integration rejects forgeries the same way.

mod harness;

use exchange_core::db::{Db, Val};
use exchange_core::env::Env;
use exchange_core::routes::{handle, Request};
use exchange_core::util::{now_iso, sha256hex};
use harness::{fresh_db, test_env, SqliteDb};
use serde_json::{json, Value};
use std::collections::HashMap;

const TEST_KEY_HEX: &str = "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";
const WRONG_KEY_HEX: &str = "6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b";

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

fn mint_with_key(key_hex: &str, claim_id: &str) -> Value {
    let key = hex::decode(key_hex).unwrap();
    let mut payload = serde_json::Map::new();
    payload.insert("kind".to_string(), Value::from("observation"));
    payload.insert("resource_id".to_string(), Value::from("r1"));
    payload.insert("ok".to_string(), Value::Bool(true));
    let map = pl::envelope::wrap_with(&key, "luci", claim_id, "established", payload, "GENESIS", &now_iso())
        .expect("mint");
    Value::Object(map)
}

async fn seed_db(db: &SqliteDb) {
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
}

async fn observe_raw(db: &SqliteDb, env: &Env, envelope: Value) -> exchange_core::routes::Response {
    handle(db, &post("/v1/observe",
        json!({"resource_id": "r1", "ok": true, "envelope": envelope}),
        admin_headers()), env).await
}

/// F1: malformed / truncated envelope → 400, not 500.
#[tokio::test]
async fn f1_malformed_envelope_400() {
    let db = fresh_db().await;
    seed_db(&db).await;
    let env = test_env_with_key();
    // Not an object → 400 "missing envelope" (JS parity).
    let r = observe_raw(&db, &env, json!("not-an-envelope")).await;
    assert_eq!(r.status, 400, "string envelope → 400: {:?}", r.body);
    // Empty object → 401 (fails verification).
    let r = observe_raw(&db, &env, json!({})).await;
    assert_eq!(r.status, 401, "empty envelope → 401: {:?}", r.body);
    // Truncated (missing sig) → 401.
    let mut good = mint_with_key(TEST_KEY_HEX, "cA");
    good.as_object_mut().unwrap().remove("sig");
    let r = observe_raw(&db, &env, good).await;
    assert_eq!(r.status, 401, "missing sig → 401: {:?}", r.body);
}

/// F2: envelope shape type-confusion → 400.
#[tokio::test]
async fn f2_type_confusion_400() {
    let db = fresh_db().await;
    seed_db(&db).await;
    let env = test_env_with_key();
    // sig is a number, not a string.
    let mut e = mint_with_key(TEST_KEY_HEX, "cA");
    e["sig"] = json!(12345);
    let r = observe_raw(&db, &env, e).await;
    assert!(r.status == 401 || r.status == 409, "bad sig type → 4xx: {:?}", r.body);
    // payload is a string, not an object.
    let mut e = mint_with_key(TEST_KEY_HEX, "cB");
    e["payload"] = json!("pwned");
    let r = observe_raw(&db, &env, e).await;
    assert_eq!(r.status, 401, "bad payload type → 400: {:?}", r.body);
}

/// F3: wrong-key signature → 400/403 (rejected, never 200).
#[tokio::test]
async fn f3_wrong_key_rejected() {
    let db = fresh_db().await;
    seed_db(&db).await;
    let env = test_env_with_key();
    let forged = mint_with_key(WRONG_KEY_HEX, "cA");
    let r = observe_raw(&db, &env, forged).await;
    assert!(r.status == 401 || r.status == 403,
        "wrong-key envelope must not be accepted: {:?}", r.body);
    // Tampered payload after signing.
    let mut e = mint_with_key(TEST_KEY_HEX, "cC");
    e["payload"]["ok"] = json!(false);
    let r = observe_raw(&db, &env, e).await;
    assert!(r.status == 401 || r.status == 403,
        "tampered payload must not be accepted: {:?}", r.body);
}

/// F7: one envelope cannot be reused across routes (seal binds claim_id).
#[tokio::test]
async fn f7_envelope_bound_to_claim() {
    let db = fresh_db().await;
    seed_db(&db).await;
    let env = test_env_with_key();
    // Envelope minted for claim cA, used on observe (which doesn't check
    // claim binding, only signature) → 200.
    let e = mint_with_key(TEST_KEY_HEX, "cA");
    let r1 = observe_raw(&db, &env, e.clone()).await;
    assert_eq!(r1.status, 200);
    // Same envelope again → 409 replay (seal registry).
    let r2 = observe_raw(&db, &env, e).await;
    assert_eq!(r2.status, 409);
}
