#!/usr/bin/env python3
"""FAKE_CLAIMANT — Worker Protocol v1 generalization proof.

An arbitrary deterministic worker implementing the same contract as the NIM
worker, with ZERO governor changes. If the full loop
  discover → evidence → verify → gates → lease → claim → execution →
  completion → receipt → verification → accounting → capacity
runs on this fake with no Exchange code changes, the worker boundary is proven:
execution capacity is replaceable without touching governance.

Deterministic ops (no network, no secrets, no intelligence):
  {"op":"sha256","data":"..."} → {"result": hex}
  {"op":"echo", ...}           → {"result": input}
  {"op":"malformed"}           → simulates provider 200 {} → FAILED_RETRYABLE
  {"op":"lying-model"}         → SUCCEEDED with model_actual="impostor-9000"
  {} (gauntlet probe)          → {"result":"probe-ok"}

Usage: FAKE_WORKER_ID / FAKE_WORKER_SECRET env, or ~/.exchange_worker_fake.
"""
import hashlib
import json
import os
import sys
import time
import urllib.error
import urllib.request

EXCHANGE = "https://lucidota-exchange.cutecity.workers.dev"
RESOURCE = os.environ.get("FAKE_RESOURCE", "fake-deterministic")
IDFILE = os.path.expanduser("~/.exchange_worker_fake")

_wid = os.environ.get("FAKE_WORKER_ID")
_wsec = os.environ.get("FAKE_WORKER_SECRET")
if not (_wid and _wsec):
    _wid, _wsec = open(IDFILE).read().strip().split("\n")[:2]


def xchg(path, body):
    req = urllib.request.Request(
        EXCHANGE + path, data=json.dumps(body).encode(), method="POST",
        headers={"Content-Type": "application/json",
                 "X-Worker-Id": _wid, "X-Worker-Secret": _wsec,
                 "User-Agent": "fake-claimant/1.0"},
    )
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return json.loads(r.read()), r.status
    except urllib.error.HTTPError as e:
        try:
            return json.loads(e.read().decode("utf-8", errors="replace")), e.code
        except Exception:
            return {"error": f"HTTP {e.code}"}, e.code


def execute(inp):
    """Deterministic, boring. No network. No decisions beyond the op table."""
    inp = inp or {}
    op = inp.get("op", "echo")
    t0 = time.time()
    if op == "sha256":
        result = hashlib.sha256(str(inp.get("data", "")).encode()).hexdigest()
        return {"worker_outcome": "SUCCEEDED", "output": {"op": op, "result": result},
                "latency_ms": int((time.time() - t0) * 1000)}
    if op == "malformed":
        # W8: provider returned 200 with no usable content. The worker must
        # not manufacture success — it reports what it mechanically observed.
        return {"worker_outcome": "FAILED_RETRYABLE", "error_class": "BAD_RESPONSE",
                "error": "provider 200 with empty choices — no content to report",
                "latency_ms": int((time.time() - t0) * 1000)}
    if op == "lying-model":
        # W9: provider executed but identifies as something else. Report the
        # discrepancy honestly; the governor binds requested vs actual.
        return {"worker_outcome": "SUCCEEDED",
                "output": {"op": op, "result": "ok"},
                "model_actual": "impostor-9000",
                "latency_ms": int((time.time() - t0) * 1000)}
    # echo (default, incl. the gauntlet's empty probe)
    return {"worker_outcome": "SUCCEEDED",
            "output": {"op": "echo", "result": inp if op == "echo" else "probe-ok"},
            "latency_ms": int((time.time() - t0) * 1000)}


def main():
    claim, _ = xchg("/v1/claim", {"resource_id": RESOURCE})
    if not claim.get("claimed"):
        print(json.dumps({"idle": True}))
        return 0
    d = claim["delegation"]
    for f in ("claim_id", "input", "lease_expiry"):
        if f not in d or d[f] is None:
            print(json.dumps({"error": f"delegation missing {f} — refusing"}))
            return 1
    res = execute(d["input"])
    done, _ = xchg("/v1/complete", {"claim_id": d["claim_id"], **res})
    print(json.dumps({"completed": claim["job_id"], "outcome": res["worker_outcome"],
                      "state": done.get("state"), "idempotent": done.get("idempotent", False)}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
