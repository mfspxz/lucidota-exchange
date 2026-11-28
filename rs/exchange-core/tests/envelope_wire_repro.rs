//! Regression test for the wasm envelope defect (2026-11-22):
//! a Python-minted lucidota-receipt/1 envelope (wire key order) must verify
//! in the EXCHANGE build graph, where serde_json has preserve_order on.
//! Before the canon fix this fails with "sig mismatch": canon::canonical
//! rebuilt maps in iteration order, and under preserve_order that is the
//! wire order, not the sorted order the signature covers.
use serde_json::Value;

const TEST_KEY_HEX: &str = "547b8c7b23f4d1e2db2b90dc7055acb6a4e5324874cd11c74d5ad4c23913360b";
const ENVELOPE_JSON: &str = r#"{"env": "lucidota-receipt/1", "ts": "2026-11-22T00:00:00Z", "actor": "repro-agent", "claim_id": "repro-claim-0001", "status": "established", "payload": {"z_note": "wire order matters", "a_count": 7, "label": "caf\u00e9 \u2713"}, "payload_hash": "3c4e01c5de388b6b370b6c81a0e79e61697b1b771018fe2cf325b59d947265f8", "prev_hash": "GENESIS", "sig": "HS256:30e701c7dd949f29b9a6d2b274fcc860e3d1d9e85dc14a8b029526367f5e65a2", "seal": "799466a4eedbe3feb6c3308c8370ce187d00525c798ef4a1a56dba3f348cb662"}"#;

fn key_bytes() -> Vec<u8> {
    (0..32)
        .map(|i| u8::from_str_radix(&TEST_KEY_HEX[i * 2..i * 2 + 2], 16).unwrap())
        .collect()
}

#[test]
fn wire_ordered_envelope_verifies() {
    let v: Value = serde_json::from_str(ENVELOPE_JSON).unwrap();
    let map = v.as_object().unwrap().clone();
    // Sanity: the fixture really is wire-ordered, not sorted.
    let keys: Vec<&String> = map.keys().collect();
    assert_ne!(keys, {
        let mut s = keys.clone();
        s.sort();
        s
    });
    pl::envelope::verify_with(&key_bytes(), &map)
        .expect("externally minted envelope must verify in the exchange graph");
}
