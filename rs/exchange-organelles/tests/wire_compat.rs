//! Cross-verify the Rust wire protocol against the Python adapter.
//! Runs both binaries on identical requests and compares full responses,
//! and compares receipt ids computed by both implementations on a battery
//! of payloads (unicode, floats, nesting).

use exchange_organelles::wire;
use serde_json::Value;
use std::process::Command;

const ADAPTER: &str = "/home/hatch/workspace/exchange/organelles/adapter.py";

fn rust_bin() -> String {
    let dir = env!("CARGO_MANIFEST_DIR");
    let ws_root = std::path::Path::new(dir)
        .parent()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    format!("{ws_root}/target/debug/exchange-organelles")
}

fn run_once(prog: &str, args: &[&str], input: &str) -> Value {
    let mut cmd = if prog.ends_with(".py") {
        let mut c = Command::new("python3");
        c.arg(prog).args(args);
        c
    } else {
        let mut c = Command::new(prog);
        c.args(args);
        c
    };
    let out = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut p| {
            use std::io::Write;
            p.stdin
                .take()
                .unwrap()
                .write_all(format!("{input}\n").as_bytes())
                .ok();
            p.wait_with_output()
        })
        .expect("spawn");
    let last = String::from_utf8_lossy(&out.stdout)
        .lines()
        .last()
        .unwrap_or("")
        .to_string();
    serde_json::from_str(&last).expect("response json")
}

fn python_receipt(payload: &Value) -> String {
    let out = Command::new("python3")
        .arg("-c")
        .arg(
            "import sys, json; sys.path.insert(0, '/home/hatch/workspace/exchange/organelles'); \
             import adapter; print(adapter._receipt_id(json.loads(sys.argv[1])))",
        )
        .arg(serde_json::to_string(payload).unwrap())
        .output()
        .expect("python");
    assert!(out.status.success(), "python failed: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Receipt formula cross-check: Rust wire::receipt_id vs Python
/// adapter._receipt_id on a battery of payloads incl. unicode and floats.
#[test]
fn receipts_match_python() {
    let cases: Vec<Value> = vec![
        serde_json::json!({"ok": true, "output": {"a": 1}, "error": null}),
        serde_json::json!({"ok": false, "output": null, "error": {"verdict": "rejected", "reason": "x"}}),
        serde_json::json!({"ok": true, "output": {"s": "h\u{e9}llo 𝄞 \t\n\"\\"}, "error": null}),
        serde_json::json!({"ok": true, "output": {"f": 1e16, "g": 1e-5, "h": 0.1, "i": 100.0}, "error": null}),
        serde_json::json!({"ok": true, "output": {"nested": {"z": [null, true, 2.5e-7]}}, "error": null}),
        serde_json::json!({"ok": true, "output": {"big": 9223372036854775807i64, "f": -0.0}, "error": null}),
    ];
    for c in cases {
        let ok = c["ok"].as_bool().unwrap();
        let rs = wire::receipt_id(ok, &c["output"], &c["error"]);
        let py = python_receipt(&c);
        assert!(py.starts_with("adapter-interim:sha256:"));
        assert_eq!(rs, py, "receipt mismatch for {c}");
    }
}

/// Full-response parity: same requests through both adapters.
#[test]
fn wire_parity_with_python_adapter() {
    let bin = rust_bin();
    let reqs = vec![
        serde_json::json!({"id": 7, "caller": "test", "capabilities": [], "codon": "ex", "op": "suspend", "input": {}}),
        serde_json::json!({"id": 9, "caller": "test", "capabilities": [], "codon": "ex", "op": "kill", "input": {}}),
        serde_json::json!({"id": 3, "caller": "test", "capabilities": [], "codon": "xx", "op": "nope", "input": {}}),
        serde_json::json!({"id": "bad", "codon": "eb", "op": "rollup", "input": {}}),
        serde_json::json!({"id": 1, "codon": "eb", "op": "rollup", "input": [1]}),
        serde_json::json!({"id": 1, "codon": "eb", "op": "rollup", "input": "x"}),
    ];
    for req in reqs {
        let arg = serde_json::to_string(&req).unwrap();
        let py = run_once(ADAPTER, &["--once"], &arg);
        let rs = run_once(&bin, &["--once"], &arg);
        assert_eq!(rs["id"], py["id"], "id mismatch for {arg}");
        assert_eq!(rs["ok"], py["ok"], "ok mismatch for {arg}");
        assert_eq!(rs["output"], py["output"], "output mismatch for {arg}");
        assert_eq!(rs["error"], py["error"], "error mismatch for {arg}");
        assert_eq!(rs["receipt"], py["receipt"], "receipt mismatch for {arg}");
    }
}

/// Bad JSON shape parity (id:null, no receipt).
#[test]
fn bad_json_parity() {
    let bin = rust_bin();
    let py = run_once(ADAPTER, &["--once"], "{not json");
    let rs = run_once(&bin, &["--once"], "{not json");
    assert_eq!(rs["id"], py["id"]);
    assert_eq!(rs["ok"], py["ok"]);
    assert!(rs["output"]["error"].as_str().unwrap().starts_with("bad json:"));
    assert!(py["output"]["error"].as_str().unwrap().starts_with("bad json:"));
    assert!(rs.get("receipt").is_none());
    assert!(py.get("receipt").is_none());
}
