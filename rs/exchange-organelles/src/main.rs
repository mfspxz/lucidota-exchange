//! exchange-organelles — Tier-B organelle surface adapter, Rust port.
//!
//! One binary:
//!   --serve [--sock PATH]   serve the wire protocol on a unix socket
//!   --once [--sock PATH]     handle one JSON request from stdin
//!   <op-subcommand> [args]   run one op natively and print its output JSON
//!
//! Op subcommands: eb-rollup, eo-govern, ed-discover, eg-sync, ei-compute,
//! ek-claim, em-submit, ew-execute, ep-probe, er-refresh.

use exchange_organelles::{ops, wire};

use serde_json::Value;
use std::io::Read;

fn usage() -> ! {
    eprintln!(
        "usage: exchange-organelles [--sock PATH] (--serve | --once | <subcommand> [args])\n\
         subcommands: eb-rollup | eo-govern | ed-discover | eg-sync | ei-compute |\n\
         \x20             ek-claim | em-submit | ew-execute | ep-probe | er-refresh"
    );
    std::process::exit(2);
}

/// Run an op from a prebuilt input Value: print output JSON on success,
/// {"ok":false,"error":{...}} on failure, exit accordingly.
fn run_op(runner: ops::Runner, input: Value) -> ! {
    match runner(input) {
        Ok(output) => {
            println!("{}", serde_json::to_string(&output).unwrap_or_default());
            std::process::exit(0);
        }
        Err(e) => {
            println!(
                "{}",
                serde_json::json!({"ok": false, "error": {"verdict": e.verdict, "reason": e.reason}})
            );
            std::process::exit(1);
        }
    }
}

/// Collect `--kebab value` / `--flag` args into an input object, mirroring
/// adapter.py's input-keys-to-flags mapping in reverse.
fn flags_to_input(args: &[String]) -> Value {
    let mut map = serde_json::Map::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(name) = a.strip_prefix("--") {
            let key = name.replace('-', "_");
            if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                let v = &args[i + 1];
                let val = if v == "true" {
                    Value::Bool(true)
                } else if v == "false" {
                    Value::Bool(false)
                } else if let Ok(n) = v.parse::<i64>() {
                    Value::from(n)
                } else if let Ok(f) = v.parse::<f64>() {
                    Value::from(f)
                } else {
                    Value::String(v.clone())
                };
                map.insert(key, val);
                i += 2;
            } else {
                map.insert(key, Value::Bool(true));
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    Value::Object(map)
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut sock = wire::default_sock();
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--sock" => {
                i += 1;
                if i >= argv.len() {
                    usage();
                }
                sock = argv[i].clone();
                i += 1;
            }
            "--serve" => {
                if let Err(e) = wire::serve(&sock) {
                    eprintln!("serve: {e}");
                    std::process::exit(1);
                }
                return;
            }
            "--once" => {
                wire::once();
                return;
            }
            _ => {
                rest = argv[i..].to_vec();
                break;
            }
        }
    }
    if rest.is_empty() {
        usage();
    }
    let sub = rest[0].as_str();
    let args = &rest[1..];
    match sub {
        "eb-rollup" => {
            let ballot_id = args.first().cloned().unwrap_or_default();
            if ballot_id.is_empty() || ballot_id.starts_with("--") {
                eprintln!("usage: exchange-organelles eb-rollup <ballot_id>");
                std::process::exit(2);
            }
            run_op(ops::eb_rollup::run, serde_json::json!({"ballot_id": ballot_id}));
        }
        "eo-govern" => run_op(ops::eo_govern::run, Value::Object(Default::default())),
        "ed-discover" => run_op(ops::ed_discover::run, Value::Object(Default::default())),
        "eg-sync" => {
            let input = flags_to_input(args); // --db PATH
            run_op(ops::eg_sync::run, input);
        }
        "ei-compute" => {
            let input = flags_to_input(args); // --latency-rate N
            run_op(ops::ei_compute::run, input);
        }
        "ek-claim" => run_op(ops::ek_claim::run, Value::Object(Default::default())),
        "em-submit" => {
            let input = flags_to_input(args); // --priority N --limit N --dry-run
            run_op(ops::em_submit::run, input);
        }
        "ew-execute" => {
            // like mint-worker.py: task JSON on stdin -> result JSON on stdout
            let mut stdin_text = String::new();
            if std::io::stdin().read_to_string(&mut stdin_text).is_err() {
                eprintln!("read stdin failed");
                std::process::exit(2);
            }
            let task: Value = match serde_json::from_str(&stdin_text) {
                Ok(v) => v,
                Err(e) => {
                    println!(
                        "{}",
                        serde_json::json!({"task_error": "permanent", "detail": format!("stdin is not JSON: {e}")})
                    );
                    std::process::exit(2);
                }
            };
            let outcome = ops::ew_execute::mint(&task);
            println!("{}", serde_json::to_string(&outcome.result).unwrap_or_default());
            std::process::exit(outcome.exit_code);
        }
        "ep-probe" => {
            let input = flags_to_input(args); // --families F1,F4 --out DIR
            run_op(ops::ep_probe::run, input);
        }
        "er-refresh" => run_op(ops::er_refresh::run, Value::Object(Default::default())),
        _ => {
            eprintln!("unknown subcommand: {sub}");
            usage();
        }
    }
}
