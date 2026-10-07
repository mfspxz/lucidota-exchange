#!/usr/bin/env python3
"""Mint claimant — Worker Protocol v1 claimant for lazy artifact minting.

PICKUP    -> POST /v1/claim on res_localswarm (worker-scoped auth)
VALIDATE  -> delegation fields present, lease not expired, task_type == mint
EXECUTE   -> mint-worker.py subprocess (pure deterministic hash check)
CAPTURE   -> validator result to exactly one terminal outcome
REPORT    -> POST /v1/complete

The Exchange IS the queue: this process pulls work via /v1/claim. No daemon.
Runs from the mint-claimant hook (every 300s) — lazy by design. Each run
claims up to 25 mint jobs, then exits.

Terminal mapping:
  validator exit 0, verdict +1/-1 -> SUCCEEDED + output.ternary_verdict
  validator exit 2 (malformed task) -> FAILED_PERMANENT
  validator timeout/crash          -> FAILED_RETRYABLE
"""
import json
import os
import subprocess
import sys
import urllib.error
import urllib.request

EXCHANGE = "https://lucidota-exchange.cutecity.workers.dev"
RESOURCE = "res_localswarm"
WORKER_ID_FILE = os.path.expanduser("~/.exchange_worker_swarm_01")
WORKER_SCRIPT = os.path.expanduser("~/workspace/exchange/mint-worker.py")
MAX_PER_RUN = 25
WORKER_TIMEOUT_S = 60


def load_cred():
    parts = open(WORKER_ID_FILE).read().strip().split("\n")
    return parts[0], parts[1]


_WID, _WSEC = load_cred()


def xchg(path, body):
    req = urllib.request.Request(
        EXCHANGE + path, data=json.dumps(body).encode(), method="POST",
        headers={"Content-Type": "application/json",
                 "X-Worker-Id": _WID, "X-Worker-Secret": _WSEC,
                 "User-Agent": "lucidota-mint-claimant/1.0"},
    )
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return json.loads(r.read()), r.status
    except urllib.error.HTTPError as e:
        try:
            return json.loads(e.read().decode("utf-8", errors="replace")), e.code
        except Exception:
            return {"error": f"HTTP {e.code}"}, e.code


def run_mint(task):
    """Run mint-worker.py as a subprocess. Returns (result_dict, outcome)."""
    try:
        p = subprocess.run(
            [sys.executable, WORKER_SCRIPT],
            input=json.dumps(task).encode(), capture_output=True,
            timeout=WORKER_TIMEOUT_S)
    except subprocess.TimeoutExpired:
        return {"error": "mint worker timeout"}, "FAILED_RETRYABLE"
    except Exception as e:
        return {"error": f"mint worker crash: {e}"}, "FAILED_RETRYABLE"
    try:
        res = json.loads(p.stdout.decode("utf-8", errors="replace"))
    except Exception:
        return {"error": "mint worker bad stdout",
                "stderr": p.stderr.decode()[:500]}, "FAILED_RETRYABLE"
    if p.returncode == 2 or res.get("task_error") == "permanent":
        return res, "FAILED_PERMANENT"
    if p.returncode != 0:
        return res, "FAILED_RETRYABLE"
    return res, "SUCCEEDED"


def main():
    done = 0
    results = []
    for _ in range(MAX_PER_RUN):
        claim, st = xchg("/v1/claim", {"resource_id": RESOURCE})
        if st != 200 or not claim.get("claimed"):
            break
        delegation = claim.get("delegation") or {}
        claim_id = delegation.get("claim_id")
        # Delegation nests spec fields FLAT: capability/input live directly
        # under delegation (no "spec" key). Mint jobs carry input.mint_id.
        task_input = (delegation.get("input") or {})
        if not claim_id:
            results.append({"mint_id": None, "outcome": "NO-CLAIM-ID"})
            break
        # DEFENSIVE: only execute mint jobs (input.mint_id present).
        # Anything else on this resource is not mine — requeue honestly via
        # FAILED_RETRYABLE so the real claimant can pick it up.
        # Never permanently fail another worker's work.
        if not task_input.get("mint_id"):
            xchg("/v1/complete", {"claim_id": claim_id,
                                  "worker_outcome": "FAILED_RETRYABLE",
                                  "output": {"error": "not a mint job — requeued"},
                                  "latency_ms": 0})
            results.append({"mint_id": None, "outcome": "SKIPPED-NOT-MINT",
                            "job_id": claim.get("job_id")})
            continue
        task = {"task_type": "mint",
                "artifact_path": task_input.get("artifact_path"),
                "expected_sha256": task_input.get("expected_sha256"),
                "mint_id": task_input.get("mint_id"),
                "local_receipt_id": task_input.get("local_receipt_id"),
                "label": task_input.get("label")}
        res, outcome = run_mint(task)
        verdict = res.get("ternary_verdict")
        body = {"claim_id": claim_id,
                "worker_outcome": outcome,
                "output": {"ternary_verdict": verdict, "mint": res} if outcome == "SUCCEEDED"
                          else {"error": res.get("error") or res.get("detail")},
                "latency_ms": (res.get("detail") or {}).get("latency_ms", 0)}
        rep, rst = xchg("/v1/complete", body)
        done += 1
        results.append({"mint_id": task["mint_id"], "outcome": outcome,
                        "verdict": verdict,
                        "receipt": (rep.get("receipt_id") if isinstance(rep, dict) else None)})
    print(json.dumps({"minted": done, "results": results}))


if __name__ == "__main__":
    main()
