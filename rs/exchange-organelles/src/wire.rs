//! Tier-B organelle wire protocol — exact port of adapter.py.
//!
//! request:  {"id":N,"caller":"...","capabilities":[],"codon":"<2-char>",
//!            "op":"<op>","input":{...}}
//! response: {"id":N,"ok":bool,"output":...|null,"error":...|null,
//!            "receipt":"adapter-interim:sha256:<hex>"}
//!
//! Receipt id = sha256 of the CANONICAL JSON of {"ok","output","error"},
//! where canonical = Python json.dumps(sort_keys=True, separators=(",",":")).
//! serde_json with the workspace-unified `preserve_order` feature does NOT
//! sort keys, so canonicalization is implemented explicitly here.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::ops::{self, OpError};

/// Global op wall-time, matching adapter.py TIMEOUT_SECS = 600.
pub const TIMEOUT_SECS: u64 = 600;

/// Python json.dumps string escaping with ensure_ascii=True:
/// control chars -> \b \f \n \r \t or \u00XX; " and \ escaped;
/// everything >= 0x7F escaped as \uXXXX (surrogate pairs above U+FFFF).
fn escape_py_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c if (c as u32) < 0x7f => out.push(c),
            c => {
                let n = c as u32;
                if n > 0xFFFF {
                    let v = n - 0x10000;
                    out.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xD800 + (v >> 10),
                        0xDC00 + (v & 0x3FF)
                    ));
                } else {
                    out.push_str(&format!("\\u{n:04x}"));
                }
            }
        }
    }
    out.push('"');
}

/// Python repr() for f64, matching json.dumps float formatting exactly.
///
/// CPython's float repr ('r' format): shortest round-trip digits, then
/// exponential notation iff decpt <= -4 or decpt > 16, else fixed notation;
/// exponent always signed, padded to at least 2 digits (1e+16, 1e-05).
/// Digits come from ryu (shortest round-trip, same digit sequence Python's
/// David Gay/Grisu path produces); only the *formatting rule* differs from
/// ryu/serde_json, so we parse ryu's output into (digits, decpt) and format
/// per the CPython rule. Verified against CPython 3.12 repr() on this box.
fn py_float(f: f64, out: &mut String) {
    if f.is_nan() {
        out.push_str("NaN");
        return;
    }
    if f.is_infinite() {
        out.push_str(if f > 0.0 { "Infinity" } else { "-Infinity" });
        return;
    }
    let neg = f < 0.0 || (f == 0.0 && f.is_sign_negative());
    let mag = f.abs();
    if neg {
        out.push('-');
    }
    if mag == 0.0 {
        out.push_str("0.0");
        return;
    }
    // raw shortest digits from ryu, e.g. "1e16" | "0.00001" | "2.5e-7" | "100.0"
    let mut buf = ryu::Buffer::new();
    let raw = buf.format(mag).to_string();
    let (mant, exp): (String, i32) = match raw.split_once('e') {
        Some((m, e)) => {
            let (sgn, ds) = match e.strip_prefix('-') {
                Some(d) => (-1, d),
                None => (1, e.strip_prefix('+').unwrap_or(e)),
            };
            (m.to_string(), sgn * ds.parse::<i32>().unwrap_or(0))
        }
        None => (raw.clone(), 0),
    };
    let point_pos = mant.find('.').unwrap_or(mant.len()) as i32;
    let all: String = mant.chars().filter(|c| *c != '.').collect();
    let k = all.chars().take_while(|c| *c == '0').count() as i32;
    let sig: String = all.chars().skip(k as usize).collect();
    // value = int(sig) * 10^(decpt - len(sig)); i.e. 0.sig * 10^decpt
    let decpt = point_pos - k + exp;
    let n = sig.len() as i32;
    if decpt <= -4 || decpt > 16 {
        // exponential: d1[.d2...]e±XX
        out.push(sig.chars().next().unwrap());
        if n > 1 {
            out.push('.');
            out.push_str(&sig[1..]);
        }
        let e10 = decpt - 1;
        out.push('e');
        out.push(if e10 < 0 { '-' } else { '+' });
        let ds = e10.abs().to_string();
        if ds.len() < 2 {
            out.push('0');
        }
        out.push_str(&ds);
    } else if decpt <= 0 {
        out.push_str("0.");
        for _ in 0..-decpt {
            out.push('0');
        }
        out.push_str(&sig);
    } else if decpt >= n {
        out.push_str(&sig);
        for _ in 0..(decpt - n) {
            out.push('0');
        }
        out.push_str(".0");
    } else {
        out.push_str(&sig[..decpt as usize]);
        out.push('.');
        out.push_str(&sig[decpt as usize..]);
    }
}

/// Canonical JSON: sorted keys, compact separators, ensure_ascii strings,
/// Python float formatting. Byte-identical to
/// json.dumps(v, sort_keys=True, separators=(",", ":")).
pub fn canonical_json(v: &Value) -> String {
    let mut out = String::new();
    write_canonical(v, &mut out);
    out
}

fn write_canonical(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                out.push_str(&i.to_string());
            } else if let Some(u) = n.as_u64() {
                out.push_str(&u.to_string());
            } else if let Some(f) = n.as_f64() {
                py_float(f, out);
            } else {
                out.push_str("null");
            }
        }
        Value::String(s) => escape_py_string(s, out),
        Value::Array(a) => {
            out.push('[');
            for (i, e) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(e, out);
            }
            out.push(']');
        }
        Value::Object(m) => {
            out.push('{');
            let mut keys: Vec<&str> = m.keys().map(|k| k.as_str()).collect();
            keys.sort_unstable();
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                escape_py_string(k, out);
                out.push(':');
                write_canonical(&m[*k], out);
            }
            out.push('}');
        }
    }
}

/// Python truthiness, for the adapter's `req.get("input") or {}` coercion:
/// falsy inputs (null, false, 0, "", [], {}) all become {} before the
/// dict check.
fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i != 0
            } else if let Some(u) = n.as_u64() {
                u != 0
            } else {
                n.as_f64().map(|f| f != 0.0).unwrap_or(false)
            }
        }
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

/// Interim receipt id: sha256 of canonical {"ok","output","error"} JSON.
pub fn receipt_id(ok: bool, output: &Value, error: &Value) -> String {
    let payload = serde_json::json!({"ok": ok, "output": output, "error": error});
    let canon = canonical_json(&payload);
    let mut h = Sha256::new();
    h.update(canon.as_bytes());
    format!("adapter-interim:sha256:{}", hex::encode(h.finalize()))
}

fn resp_value(rid: &Value, ok: bool, output: Value, error: Value) -> Value {
    let receipt = receipt_id(ok, &output, &error);
    serde_json::json!({
        "id": rid,
        "ok": ok,
        "output": output,
        "error": error,
        "receipt": receipt,
    })
}

fn rejected(rid: &Value, reason: String) -> Value {
    resp_value(
        rid,
        false,
        Value::Null,
        serde_json::json!({"verdict": "rejected", "reason": reason}),
    )
}

/// Handle one decoded request object. Mirrors adapter.handle() exactly,
/// including the pass-through of a malformed `id`.
pub fn handle(req: &Value) -> Value {
    let rid = req.get("id").cloned().unwrap_or(Value::Null);
    let codon = req.get("codon").and_then(|c| c.as_str()).unwrap_or("");
    let op = req.get("op").and_then(|o| o.as_str()).unwrap_or("");
    // adapter.py: op_input = req.get("input") or {}
    let input: Value = match req.get("input") {
        Some(v) if py_truthy(v) => v.clone(),
        _ => Value::Object(Default::default()),
    };

    // Python: isinstance(rid, int) — and bool is a subclass of int.
    let rid_is_int = rid.as_i64().is_some() || rid.as_u64().is_some() || rid.is_boolean();
    if !rid_is_int || codon.is_empty() || op.is_empty() || !input.is_object() {
        return rejected(&rid, "malformed request".to_string());
    }

    if op == "suspend" {
        return resp_value(
            &rid,
            true,
            serde_json::json!({
                "checkpoint": "ephemeral",
                "note": "in-flight op drained; adapter state is ephemeral",
            }),
            Value::Null,
        );
    }
    if op == "kill" {
        return resp_value(
            &rid,
            true,
            serde_json::json!({"final": true, "note": "final receipt; adapter exiting"}),
            Value::Null,
        );
    }

    let runner = match ops::lookup(codon, op) {
        Some(r) => r,
        None => {
            return rejected(
                &rid,
                format!("unknown op {codon}/{op}"),
            )
        }
    };

    // Run the op on a worker thread with the global wall-time budget,
    // mirroring the adapter's 600s subprocess timeout.
    let (tx, rx) = std::sync::mpsc::channel();
    let input_clone = input.clone();
    thread::spawn(move || {
        let _ = tx.send(runner(input_clone));
    });
    match rx.recv_timeout(Duration::from_secs(TIMEOUT_SECS)) {
        Ok(Ok(output)) => resp_value(&rid, true, output, Value::Null),
        Ok(Err(OpError { verdict, reason })) => resp_value(
            &rid,
            false,
            Value::Null,
            serde_json::json!({"verdict": verdict, "reason": reason}),
        ),
        Err(_) => resp_value(
            &rid,
            false,
            Value::Null,
            serde_json::json!({
                "verdict": "timeout",
                "reason": format!("timeout after {TIMEOUT_SECS}s (suspend-first: host retries)"),
            }),
        ),
    }
}

/// Handle one raw line; bad JSON gets the adapter's id:null error shape
/// (no receipt — matching adapter.py).
pub fn handle_line(line: &str) -> Value {
    match serde_json::from_str::<Value>(line) {
        Ok(req) => handle(&req),
        Err(e) => serde_json::json!({
            "id": Value::Null,
            "ok": false,
            "output": {"error": format!("bad json: {e}")},
        }),
    }
}

pub fn default_sock() -> String {
    std::env::var("ORGANELLE_SOCK").unwrap_or_else(|_| {
        let rt = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
        format!("{rt}/organelle-host.sock")
    })
}

fn handle_conn(stream: UnixStream, shutdown: &Arc<AtomicBool>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(_) => break,
        }
        if line.trim().is_empty() {
            continue;
        }
        let resp = handle_line(&line);
        let is_final = resp
            .get("output")
            .and_then(|o| o.get("final"))
            .and_then(|f| f.as_bool())
            .unwrap_or(false);
        let mut out = serde_json::to_string(&resp).unwrap_or_default();
        out.push('\n');
        let stream = reader.get_mut();
        if stream.write_all(out.as_bytes()).is_err() {
            break;
        }
        if is_final {
            shutdown.store(true, Ordering::SeqCst);
            break;
        }
    }
}

/// Serve forever on the unix socket, mirroring adapter.serve().
pub fn serve(sock_path: &str) -> std::io::Result<()> {
    let _ = std::fs::remove_file(sock_path);
    let listener = UnixListener::bind(sock_path)?;
    listener.set_nonblocking(true)?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let flag = shutdown.clone();
    let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, flag.clone());
    let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, flag);

    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let sd = shutdown.clone();
                thread::spawn(move || handle_conn(stream, &sd));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                eprintln!("accept error: {e}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
    let _ = std::fs::remove_file(sock_path);
    Ok(())
}

/// --once: read one JSON line from stdin, print the response.
pub fn once() {
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    let resp = handle_line(&line);
    println!("{}", serde_json::to_string(&resp).unwrap_or_default());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_matches_python_basics() {
        // cross-checked against the Python adapter in tests/wire_compat.rs;
        // spot assertions here for the tricky cases.
        assert_eq!(canonical_json(&serde_json::json!({"b":1,"a":2})), r#"{"a":2,"b":1}"#);
        assert_eq!(canonical_json(&serde_json::json!({"x":"héllo"})), r#"{"x":"h\u00e9llo"}"#);
        assert_eq!(canonical_json(&serde_json::json!({"e":"𝄞"})), r#"{"e":"\ud834\udd1e"}"#);
        assert_eq!(canonical_json(&serde_json::json!(1e16)), "1e+16");
        assert_eq!(canonical_json(&serde_json::json!(1e-5)), "1e-05");
        assert_eq!(canonical_json(&serde_json::json!(2.5e-7)), "2.5e-07");
        assert_eq!(canonical_json(&serde_json::json!(1e100)), "1e+100");
        assert_eq!(canonical_json(&serde_json::json!(0.1)), "0.1");
        assert_eq!(
            canonical_json(&serde_json::json!({"a":"b\tc\nd"})),
            r#"{"a":"b\tc\nd"}"#
        );
        assert_eq!(
            canonical_json(&serde_json::json!({"n":null,"t":true,"f":false})),
            r#"{"f":false,"n":null,"t":true}"#
        );
    }

    #[test]
    fn malformed_requests_rejected() {
        for req in [
            serde_json::json!({"id": "7", "codon": "eb", "op": "rollup", "input": {}}),
            serde_json::json!({"codon": "eb", "op": "rollup", "input": {}}),
            serde_json::json!({"id": 1, "op": "rollup", "input": {}}),
            serde_json::json!({"id": 1, "codon": "eb", "input": {}}),
            serde_json::json!({"id": 1, "codon": "eb", "op": "rollup", "input": [1]}),
            serde_json::json!({"id": 1, "codon": "eb", "op": "rollup", "input": "x"}),
            serde_json::json!({"id": 1, "codon": "eb", "op": "rollup", "input": 5}),
            serde_json::json!({"id": 1.5, "codon": "eb", "op": "rollup", "input": {}}),
        ] {
            let r = handle(&req);
            assert_eq!(r["ok"], false, "{req}");
            assert_eq!(r["error"]["verdict"], "rejected", "{req}");
            assert!(r["receipt"].as_str().unwrap().starts_with("adapter-interim:sha256:"), "{req}");
        }
        // id passes through verbatim even when malformed
        let r = handle(&serde_json::json!({"id": "seven", "codon": "eb", "op": "rollup", "input": {}}));
        assert_eq!(r["id"], "seven");
    }

    #[test]
    fn falsy_input_coerces_to_empty_object() {
        // adapter.py: req.get("input") or {} — [] is falsy, so it dispatches
        // with {}; the op then rejects for its own reasons, never "malformed".
        let r = handle(&serde_json::json!({"id": 1, "codon": "eb", "op": "rollup", "input": []}));
        assert_ne!(r["error"]["reason"], "malformed request");
        assert_eq!(r["error"]["verdict"], "rejected");
        assert_eq!(r["error"]["reason"], "ballot_id required");
    }

    #[test]
    fn unknown_op_rejected() {
        let r = handle(&serde_json::json!({"id": 3, "codon": "xx", "op": "nope", "input": {}}));
        assert_eq!(r["ok"], false);
        assert_eq!(r["error"]["verdict"], "rejected");
        assert_eq!(r["error"]["reason"], "unknown op xx/nope");
    }

    #[test]
    fn suspend_semantics() {
        let r = handle(&serde_json::json!({"id": 7, "codon": "ex", "op": "suspend", "input": {}}));
        assert_eq!(r["id"], 7);
        assert_eq!(r["ok"], true);
        assert_eq!(r["output"]["checkpoint"], "ephemeral");
        assert!(r["output"]["note"].as_str().unwrap().contains("ephemeral"));
        assert_eq!(r["error"], Value::Null);
        assert!(r["receipt"].as_str().unwrap().starts_with("adapter-interim:sha256:"));
    }

    #[test]
    fn kill_semantics() {
        let r = handle(&serde_json::json!({"id": 9, "codon": "ex", "op": "kill", "input": {}}));
        assert_eq!(r["ok"], true);
        assert_eq!(r["output"]["final"], true);
        assert!(r["receipt"].as_str().unwrap().starts_with("adapter-interim:sha256:"));
    }

    #[test]
    fn bad_json_shape() {
        let r = handle_line("{not json");
        assert_eq!(r["id"], Value::Null);
        assert_eq!(r["ok"], false);
        assert!(r["output"]["error"].as_str().unwrap().starts_with("bad json:"));
        assert!(r.get("receipt").is_none());
    }

    #[test]
    fn receipt_deterministic() {
        let a = receipt_id(true, &serde_json::json!({"x":1}), &Value::Null);
        let b = receipt_id(true, &serde_json::json!({"x":1}), &Value::Null);
        assert_eq!(a, b);
        let c = receipt_id(true, &serde_json::json!({"x":2}), &Value::Null);
        assert_ne!(a, c);
        assert_eq!(a.len(), "adapter-interim:sha256:".len() + 64);
    }
}
