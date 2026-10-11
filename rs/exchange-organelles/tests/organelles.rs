//! Port of exchange/organelles/test_organelles.py assertions:
//! manifests validate, dispatch covers every manifest (codon,op),
//! execution.json is complete, --once suspend smoke, no secret shapes.

use std::collections::HashSet;
use std::process::Command;

const ORGANELLES: &str = "/home/hatch/workspace/exchange/organelles";

fn rust_bin() -> String {
    let dir = env!("CARGO_MANIFEST_DIR");
    let ws_root = std::path::Path::new(dir)
        .parent()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    format!("{ws_root}/target/debug/exchange-organelles")
}

fn load_manifests() -> Vec<(String, serde_json::Value)> {
    let mut out = Vec::new();
    let rd = std::fs::read_dir(ORGANELLES).expect("organelles dir");
    let mut paths: Vec<String> = rd
        .flatten()
        .map(|e| e.path().to_string_lossy().into_owned())
        .filter(|p| p.ends_with(".manifest.json"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no manifests found");
    for p in paths {
        let m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).expect("read manifest")).expect("manifest json");
        out.push((p, m));
    }
    out
}

#[test]
fn manifests_ok() {
    for (p, m) in load_manifests() {
        let unit = std::path::Path::new(&p)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .replace(".manifest.json", "");
        let name = m["name"].as_str().unwrap_or("");
        assert!(
            name.ends_with(&format!("-{unit}")) || name == format!("exchange-{unit}"),
            "name/file mismatch: {p}"
        );
        for f in [
            "schema_version",
            "name",
            "codon",
            "version",
            "description",
            "contracts",
            "authority",
            "transports",
            "receipt_policy",
        ] {
            assert!(m.get(f).is_some(), "{p} missing {f}");
        }
        assert_eq!(m["schema_version"], "1");
        let caps: HashSet<&str> = m["authority"]["granted_capabilities"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        for op in m["contracts"].as_array().cloned().unwrap_or_default() {
            let mc = op
                .get("mutation_class")
                .and_then(|v| v.as_str())
                .or_else(|| m["authority"]["mutation_class"].as_str())
                .unwrap_or("");
            if mc == "write" || mc == "external_effect" {
                let req = op.get("required_capability").and_then(|v| v.as_str());
                assert!(
                    req.map(|r| caps.contains(r)).unwrap_or(false),
                    "{p} op {} {mc} lacks granted capability",
                    op["op"]
                );
            }
        }
    }
}

#[test]
fn dispatch_covers_manifests() {
    // the Rust ops::DISPATCH table must cover every (codon, op) contract
    // in the manifests. Checked via the library registry — no op executes.
    use exchange_organelles::ops;
    for (p, m) in load_manifests() {
        let codon = m["codon"].as_str().unwrap().to_string();
        for op in m["contracts"].as_array().cloned().unwrap_or_default() {
            let opname = op["op"].as_str().unwrap().to_string();
            assert!(
                ops::lookup(&codon, &opname).is_some(),
                "{p}: {codon}/{opname} missing from Rust DISPATCH"
            );
        }
    }
    // and exactly the 10 expected units are registered
    assert_eq!(ops::DISPATCH.len(), 10);
}

#[test]
fn exec_json_ok() {
    let e: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(format!("{ORGANELLES}/execution.json")).expect("read execution.json"),
    )
    .expect("execution json");
    for k in [
        "filesystem",
        "network",
        "secrets",
        "resource_envelope",
        "kill_semantics",
        "sandbox",
        "repo_head",
    ] {
        assert!(e.get(k).is_some(), "execution.json missing {k}");
    }
    let env = &e["resource_envelope"];
    for k in ["cpu_cores", "mem_mb", "wall_time_secs", "io_mb"] {
        assert!(env.get(k).is_some(), "envelope missing {k}");
    }
    assert_eq!(e["kill_semantics"]["final_receipt"], true);
}

#[test]
fn adapter_smoke_suspend() {
    let bin = rust_bin();
    let req = serde_json::json!({"id": 7, "caller": "test", "capabilities": [], "codon": "ex", "op": "suspend", "input": {}});
    let out = Command::new(&bin)
        .arg("--once")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut proc| {
            use std::io::Write;
            proc.stdin
                .take()
                .unwrap()
                .write_all(format!("{}\n", serde_json::to_string(&req).unwrap()).as_bytes())
                .ok();
            proc.wait_with_output()
        })
        .expect("spawn");
    let last = String::from_utf8_lossy(&out.stdout)
        .lines()
        .last()
        .unwrap_or("")
        .to_string();
    let resp: serde_json::Value = serde_json::from_str(&last).expect("resp json");
    assert_eq!(resp["id"], 7);
    assert_eq!(resp["ok"], true);
    assert!(resp["receipt"].as_str().unwrap().starts_with("adapter-interim:sha256:"));
    assert_eq!(resp["error"], serde_json::Value::Null);
}

#[test]
fn no_secrets_in_manifests() {
    let pat = regex::Regex::new(
        r"(?i)(sk-|ghp_|xox[bap]-|AKIA|BEGIN [A-Z ]*PRIVATE KEY|-----BEGIN)",
    )
    .unwrap();
    for (p, m) in load_manifests() {
        let blob = serde_json::to_string(&m).unwrap();
        assert!(!pat.is_match(&blob), "secret-shaped material in {p}");
        for op in m["contracts"].as_array().cloned().unwrap_or_default() {
            assert!(op["op"].as_str().unwrap_or("").len() <= 64);
        }
    }
}
