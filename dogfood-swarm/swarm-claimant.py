#!/usr/bin/env python3
"""Swarm claimant — Worker Protocol v1 implementation for cognitive work.

BORING BY DESIGN. Five steps, same as nim-claimant.py:
  PICKUP    → POST /v1/claim (worker-scoped auth — never the Exchange key)
  VALIDATE  → delegation fields present, lease not expired
  EXECUTE   → fan out to swarm-worker.py subprocess (the validator);
               the model executes the probe, deterministic code judges
  CAPTURE   → map validator result to exactly one terminal outcome
  REPORT    → POST /v1/complete
  GRAPH     → append the verifies lifelog_edge (exact DDL schema)

The Exchange IS the queue: this process pulls work via /v1/claim. No
daemon watches anything. Evidence subtasks are graph nodes; the
governor's deferred-delegation queue is the work queue.

Terminal mapping (execution vs epistemics are separate):
  validator exit 0, verdict +1/-1/0 → SUCCEEDED + output.ternary_verdict
  validator exit 2 (malformed task)  → FAILED_PERMANENT
  validator timeout/crash            → FAILED_RETRYABLE
"""
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

EXCHANGE = "https://lucidota-exchange.cutecity.workers.dev"
RESOURCE = "res_localswarm"
WORKER_ID_FILE = os.path.expanduser("~/.exchange_worker_swarm_01")
WORKER_SCRIPT = os.path.expanduser("~/workspace/exchange/dogfood-swarm/swarm-worker.py")
GRAPH_DROPS = os.path.expanduser("~/workspace/exchange/dogfood-swarm/graph-drops")
WORKER_TIMEOUT_S = 180

_wid, _wsec = open(WORKER_ID_FILE).read().strip().split("\n")[:2]


def xchg(path, body):
    req = urllib.request.Request(
        EXCHANGE + path, data=json.dumps(body).encode(), method="POST",
        headers={"Content-Type": "application/json",
                 "X-Worker-Id": _wid, "X-Worker-Secret": _wsec,
                 "User-Agent": "lucidota-swarm-claimant/1.0"},
    )
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return json.loads(r.read()), r.status
    except urllib.error.HTTPError as e:
        try:
            return json.loads(e.read().decode("utf-8", errors="replace")), e.code
        except Exception:
            return {"error": f"HTTP {e.code}"}, e.code


def append_edge(edge):
    os.makedirs(GRAPH_DROPS, exist_ok=True)
    path = os.path.join(GRAPH_DROPS, "lifelog_edge.jsonl")
    with open(path, "a") as f:
        f.write(json.dumps(edge, sort_keys=True) + "\n")
    return path


def run_one():
    """One PICKUP→VALIDATE→EXECUTE→CAPTURE→REPORT→GRAPH cycle.
    Returns True if work was done."""
    claim, _ = xchg("/v1/claim", {"resource_id": RESOURCE})
    if not claim.get("claimed"):
        return False
    d = claim["delegation"]
    for f in ("claim_id", "input", "lease_expiry", "policy_snapshot"):
        if f not in d or d[f] is None:
            print(json.dumps({"error": f"delegation missing {f} — refusing"}))
            return True
    if d["lease_expiry"] < time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()):
        print(json.dumps({"error": "delegation already expired — refusing"}))
        return True
    task = d["input"] or {}

    # EXECUTE — fan out to the validator subprocess. Timeout-bound; the
    # governor replans on retryable failure. No retries here.
    outcome, error_class, err, output = "FAILED_RETRYABLE", "WORKER_TIMEOUT", None, None
    try:
        p = subprocess.run(
            [sys.executable, WORKER_SCRIPT],
            input=json.dumps(task).encode(), capture_output=True,
            timeout=WORKER_TIMEOUT_S)
        try:
            res = json.loads(p.stdout.decode("utf-8", errors="replace") or "{}")
        except Exception:
            res = {}
        if p.returncode == 0 and "ternary_verdict" in res:
            # Validator ran cleanly. Execution SUCCEEDED — the ternary
            # verdict (even -1 or 0) is the epistemic outcome, not a failure.
            outcome, error_class, err = "SUCCEEDED", None, None
            output = {"ternary_verdict": res["ternary_verdict"],
                      "check": res.get("detail"),
                      "graph_task_id": task.get("graph_task_id"),
                      "claim_id": task.get("claim_id")}
        elif p.returncode == 2 or res.get("task_error") == "permanent":
            outcome, error_class = "FAILED_PERMANENT", "BAD_TASK"
            err = str(res.get("detail", "validator rejected task"))[:300]
        else:
            err = str(res.get("detail") or p.stderr.decode()[:200] or
                      f"exit={p.returncode}")[:300]
    except subprocess.TimeoutExpired:
        err = f"validator timeout after {WORKER_TIMEOUT_S}s"
    except Exception as e:
        err = f"validator spawn failed: {str(e)[:200]}"

    # REPORT — exactly one terminal outcome.
    body = {"claim_id": d["claim_id"], "worker_outcome": outcome,
            "latency_ms": 0, "model_actual": "swarm-validator/1.0"}
    if error_class:
        body["error_class"] = error_class
    if err:
        body["error"] = err
    if output:
        body["output"] = output
    done, _ = xchg("/v1/complete", body)
    receipt_id = done.get("receipt_id")

    # GRAPH — the verifies edge, bound to the Exchange receipt. Exact DDL
    # schema; SurrealDB-ingestible. The graph decides; this only records.
    if output and receipt_id:
        v = output["ternary_verdict"]
        append_edge({
            "table": "lifelog_edge",
            "in": task.get("graph_task_id"), "out": task.get("claim_id"),
            "edge_kind": "verifies",
            "claimant": "worker:swarm-claimant-01",
            "source": "system", "necessity": "required", "verdict": v,
            "receipt_id": receipt_id,
            "hash": "cas:" + (output.get("check") or {}).get("output_hash", "none"),
            "valid_from": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        })
    print(json.dumps({"completed": claim["job_id"], "outcome": outcome,
                      "ternary": (output or {}).get("ternary_verdict"),
                      "receipt_id": receipt_id,
                      "state": done.get("state")}, sort_keys=True), flush=True)
    return True


def main():
    did = run_one()
    print(json.dumps({"did_work": did}))


if __name__ == "__main__":
    main()
