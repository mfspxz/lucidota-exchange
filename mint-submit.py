#!/usr/bin/env python3
"""Batch-submit mint jobs from ~/workspace/mint-queue.jsonl to the Exchange.

Reads queued items (highest priority first), verifies the artifact file
exists locally, and POSTs each as a /v1/job with capability "cognitive"
(-> res_localswarm, where mint-claimant.py picks them up).

Idempotency: idempotency_key = mint_id, so re-runs dedupe instead of
duplicating. Respects 429 backpressure (stops the batch, keeps queue).

Budget-aware: prints rows-affecting counts; caller decides batch size.
Usage: mint-submit.py [--priority N] [--limit N] [--dry-run]
"""
import json
import os
import sys
import urllib.error
import urllib.request

EXCHANGE = "https://lucidota-exchange.cutecity.workers.dev"
QUEUE = os.path.expanduser("~/workspace/mint-queue.jsonl")
KEY_FILE = os.path.expanduser("~/.exchange_key")


def load_key():
    return open(KEY_FILE).read().strip()


def post_job(key, spec):
    req = urllib.request.Request(
        EXCHANGE + "/v1/job", data=json.dumps(spec).encode(), method="POST",
        headers={"Content-Type": "application/json",
                 "Authorization": f"Bearer {key}",
                 "User-Agent": "lucidota-mint-submit/1.0"})
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return json.loads(r.read()), r.status
    except urllib.error.HTTPError as e:
        try:
            return json.loads(e.read().decode("utf-8", errors="replace")), e.code
        except Exception:
            return {"error": f"HTTP {e.code}"}, e.code


def main():
    args = sys.argv[1:]
    dry = "--dry-run" in args
    prio = None
    limit = None
    for i, a in enumerate(args):
        if a == "--priority" and i + 1 < len(args):
            prio = int(args[i + 1])
        if a == "--limit" and i + 1 < len(args):
            limit = int(args[i + 1])

    items = [json.loads(l) for l in open(QUEUE) if l.strip()]
    todo = [x for x in items if x["status"] == "queued"
            and (prio is None or x["priority"] == prio)]
    todo.sort(key=lambda x: (x["priority"], x["mint_id"]))
    if limit:
        todo = todo[:limit]

    # Pre-verify artifacts locally (no point submitting for missing files)
    verified = []
    missing = []
    for x in todo:
        if os.path.isfile(x["artifact_path"]):
            verified.append(x)
        else:
            missing.append(x["mint_id"])

    key = load_key()
    submitted, deduped, backpressured, failed = 0, 0, 0, 0
    results = []
    for x in verified:
        spec = {
            "organelle": "mint-worker",
            "capability": "cognitive",
            "input": {
                "task_type": "mint",
                "mint_id": x["mint_id"],
                "artifact_path": x["artifact_path"],
                "expected_sha256": x["sha256"],
                "local_receipt_id": x["local_receipt_id"],
                "label": x["label"],
            },
            "data_class": "public",
            "max_cost": 0,
            "priority": 6,
            "idempotency_key": x["mint_id"],
        }
        if dry:
            results.append({"mint_id": x["mint_id"], "dry": True})
            continue
        body, st = post_job(key, spec)
        if st == 429:
            backpressured += 1
            results.append({"mint_id": x["mint_id"], "outcome": "BACKPRESSURE-429"})
            break  # stop the batch; queue keeps the rest
        if st != 200:
            failed += 1
            results.append({"mint_id": x["mint_id"], "outcome": "FAILED",
                            "error": str(body)[:200]})
            continue
        if body.get("deduplicated"):
            deduped += 1
        else:
            submitted += 1
        results.append({"mint_id": x["mint_id"], "outcome": "SUBMITTED",
                        "job_id": body.get("job_id"), "state": body.get("state")})

    print(json.dumps({
        "submitted": submitted, "deduped": deduped,
        "backpressured": backpressured, "failed": failed,
        "missing_artifacts": missing, "dry_run": dry,
        "results": results,
    }))


if __name__ == "__main__":
    main()
