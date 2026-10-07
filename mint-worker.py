#!/usr/bin/env python3
"""Mint worker — PURELY DETERMINISTIC artifact minting for the Exchange.

No LLM. No model. No judgment. This process verifies that a local artifact
file exists and its sha256 matches the expected hash, then reports a
ternary verdict. The Exchange receipt this produces is the live,
governor-recorded proof that the artifact existed with that hash at mint
time, linked to the original local receipt id.

This is NOT determinism theater: replace this script with `sha256sum` and
a shell comparison and the verdict logic is unchanged. There is no model
in the loop at all.

Protocol: task JSON on stdin -> result JSON on stdout.
  task = {"task_type": "mint",
          "artifact_path": "/abs/path",
          "expected_sha256": "<hex>",
          "mint_id": "mint-...",
          "local_receipt_id": "rcpt-...|null",
          "label": "human label"}
  Exit 0 = ran cleanly. verdict +1 = hash matches (proven).
  verdict -1 = missing file or hash mismatch (disproven).
  Exit 2 = malformed task (permanent).
"""
import hashlib
import json
import os
import sys
import time


def mint(task):
    t0 = time.time()
    apath = task.get("artifact_path")
    expect = (task.get("expected_sha256") or "").lower()
    if not apath or not expect:
        return {"task_error": "permanent",
                "detail": "artifact_path and expected_sha256 required"}, 2
    if not os.path.isfile(apath):
        return {"ternary_verdict": -1,
                "detail": {"reason": "artifact missing",
                           "artifact_path": apath,
                           "mint_id": task.get("mint_id")}}, 0
    h = hashlib.sha256()
    with open(apath, "rb") as f:
        for chunk in iter(lambda: f.read(65536), b""):
            h.update(chunk)
    actual = h.hexdigest()
    ok = actual == expect
    return {"ternary_verdict": 1 if ok else -1,
            "detail": {"reason": "hash match" if ok else "HASH MISMATCH",
                       "artifact_path": apath,
                       "mint_id": task.get("mint_id"),
                       "local_receipt_id": task.get("local_receipt_id"),
                       "label": task.get("label"),
                       "expected_sha256": expect,
                       "actual_sha256": actual,
                       "bytes": os.path.getsize(apath),
                       "latency_ms": int((time.time() - t0) * 1000)}}, 0


def main():
    try:
        task = json.load(sys.stdin)
    except Exception as e:
        print(json.dumps({"task_error": "permanent",
                          "detail": f"stdin is not JSON: {e}"}))
        return 2
    if task.get("task_type") != "mint":
        print(json.dumps({"task_error": "permanent",
                          "detail": f"unknown task_type: {task.get('task_type')}"}))
        return 2
    res, code = mint(task)
    print(json.dumps(res))
    return code


if __name__ == "__main__":
    sys.exit(main())
