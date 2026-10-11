//! ew/execute — PURELY DETERMINISTIC artifact minting.
//! Port of mint-worker.py. No LLM, no judgment: verify a local artifact
//! file exists and its sha256 matches the expected hash, then report a
//! ternary verdict.
//!
//! As an organelle op, `input` IS the task object. The CLI subcommand
//! reads the task JSON from stdin exactly like the Python script.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::time::Instant;

use super::OpError;

/// Exit-code semantics mirror the Python script: 0 = ran cleanly,
/// 2 = malformed task (permanent).
pub struct MintOutcome {
    pub result: Value,
    pub exit_code: i32,
}

pub fn mint(task: &Value) -> MintOutcome {
    let t0 = Instant::now();
    let apath = task.get("artifact_path").and_then(|v| v.as_str()).unwrap_or("");
    let expect = task
        .get("expected_sha256")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    if apath.is_empty() || expect.is_empty() {
        return MintOutcome {
            result: serde_json::json!({
                "task_error": "permanent",
                "detail": "artifact_path and expected_sha256 required",
            }),
            exit_code: 2,
        };
    }
    if !std::path::Path::new(apath).is_file() {
        return MintOutcome {
            result: serde_json::json!({
                "ternary_verdict": -1,
                "detail": {
                    "reason": "artifact missing",
                    "artifact_path": apath,
                    "mint_id": task.get("mint_id"),
                },
            }),
            exit_code: 0,
        };
    }
    let mut h = Sha256::new();
    let mut f = match File::open(apath) {
        Ok(f) => f,
        Err(e) => {
            return MintOutcome {
                result: serde_json::json!({
                    "ternary_verdict": -1,
                    "detail": {
                        "reason": format!("artifact unreadable: {e}"),
                        "artifact_path": apath,
                        "mint_id": task.get("mint_id"),
                    },
                }),
                exit_code: 0,
            }
        }
    };
    let mut buf = [0u8; 65536];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => h.update(&buf[..n]),
            Err(e) => {
                return MintOutcome {
                    result: serde_json::json!({
                        "ternary_verdict": -1,
                        "detail": {
                            "reason": format!("artifact read error: {e}"),
                            "artifact_path": apath,
                            "mint_id": task.get("mint_id"),
                        },
                    }),
                    exit_code: 0,
                }
            }
        }
    }
    let actual = hex::encode(h.finalize());
    let ok = actual == expect;
    let bytes = std::fs::metadata(apath).map(|m| m.len()).unwrap_or(0);
    MintOutcome {
        result: serde_json::json!({
            "ternary_verdict": if ok { 1 } else { -1 },
            "detail": {
                "reason": if ok { "hash match" } else { "HASH MISMATCH" },
                "artifact_path": apath,
                "mint_id": task.get("mint_id"),
                "local_receipt_id": task.get("local_receipt_id"),
                "label": task.get("label"),
                "expected_sha256": expect,
                "actual_sha256": actual,
                "bytes": bytes,
                "latency_ms": t0.elapsed().as_millis() as u64,
            },
        }),
        exit_code: 0,
    }
}

pub fn run(input: Value) -> Result<Value, OpError> {
    if input.get("task_type").and_then(|v| v.as_str()) != Some("mint") {
        return Err(OpError::rejected(format!(
            "unknown task_type: {}",
            input.get("task_type").unwrap_or(&Value::Null)
        )));
    }
    Ok(mint(&input).result)
}

/// Terminal mapping for ek_claim, mirroring mint-claimant.run_mint:
/// exit 2 / task_error permanent -> FAILED_PERMANENT;
/// exit != 0 -> FAILED_RETRYABLE; exit 0 -> SUCCEEDED.
pub fn terminal_outcome(task: &Value) -> (Value, &'static str) {
    let out = mint(task);
    let res = out.result;
    if out.exit_code == 2 || res.get("task_error").and_then(|v| v.as_str()) == Some("permanent") {
        (res, "FAILED_PERMANENT")
    } else if out.exit_code != 0 {
        (res, "FAILED_RETRYABLE")
    } else {
        (res, "SUCCEEDED")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_artifact(content: &[u8]) -> (tempfile::TempDir, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifact.bin");
        let mut f = File::create(&path).unwrap();
        f.write_all(content).unwrap();
        let mut h = Sha256::new();
        h.update(content);
        let hex_digest = hex::encode(h.finalize());
        (dir, path.to_string_lossy().into_owned(), hex_digest)
    }

    fn task(path: &str, sha: &str) -> Value {
        serde_json::json!({
            "task_type": "mint",
            "artifact_path": path,
            "expected_sha256": sha,
            "mint_id": "mint-test-1",
            "local_receipt_id": "rcpt-1",
            "label": "test",
        })
    }

    #[test]
    fn hash_match_verdict_plus_one() {
        let (_d, path, sha) = tmp_artifact(b"hello world");
        let out = mint(&task(&path, &sha));
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.result["ternary_verdict"], 1);
        assert_eq!(out.result["detail"]["reason"], "hash match");
        assert_eq!(out.result["detail"]["bytes"], 11);
        assert!(out.result["detail"]["latency_ms"].as_u64().is_some());
    }

    #[test]
    fn hash_mismatch_verdict_minus_one() {
        let (_d, path, _sha) = tmp_artifact(b"hello world");
        let out = mint(&task(&path, &"0".repeat(64)));
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.result["ternary_verdict"], -1);
        assert_eq!(out.result["detail"]["reason"], "HASH MISMATCH");
    }

    #[test]
    fn missing_file_verdict_minus_one() {
        let out = mint(&task("/nonexistent/artifact.bin", &"a".repeat(64)));
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.result["ternary_verdict"], -1);
        assert_eq!(out.result["detail"]["reason"], "artifact missing");
    }

    #[test]
    fn missing_fields_permanent_exit_2() {
        let out = mint(&serde_json::json!({"task_type": "mint"}));
        assert_eq!(out.exit_code, 2);
        assert_eq!(out.result["task_error"], "permanent");
        let (res, outcome) = terminal_outcome(&serde_json::json!({"task_type": "mint"}));
        assert_eq!(outcome, "FAILED_PERMANENT");
        assert_eq!(res["task_error"], "permanent");
    }

    #[test]
    fn wrong_task_type_rejected() {
        let r = run(serde_json::json!({"task_type": "train"}));
        assert!(matches!(r, Err(OpError { verdict: "rejected", .. })));
    }

    #[test]
    fn terminal_outcome_mapping() {
        let (_d, path, sha) = tmp_artifact(b"data");
        let t = task(&path, &sha);
        let (_, o) = terminal_outcome(&t);
        assert_eq!(o, "SUCCEEDED");
        let t2 = task("/nonexistent/x", &"b".repeat(64));
        let (_, o2) = terminal_outcome(&t2);
        assert_eq!(o2, "SUCCEEDED"); // ran cleanly, verdict -1
        let (_, o3) = terminal_outcome(&serde_json::json!({"task_type": "mint"}));
        assert_eq!(o3, "FAILED_PERMANENT");
    }
}
