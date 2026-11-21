#!/usr/bin/env python3
"""Deploy Exchange worker v0.7.0 (multipart: worker.js + envelope.js).

WHY NOT deploy-governor.py / cf.py deploy-worker: both upload ONLY
worker.js (no envelope.js module — the deployed worker imports it) and
deploy-governor.py also PUTs the cron schedule to */5 (the live trigger
is */30 since Oct 9; the work order forbids cron changes). This script:
  * uploads worker.js + envelope.js as ES modules in one multipart PUT,
  * sends ONLY the D1 binding in metadata (EXCHANGE_KEY + PL_SIG_KEY are
    script secrets — they persist across uploads; never touched here),
  * NEVER touches schedules: fetches the trigger list before and after
    and aborts the report if it changed,
  * verifies the deployed version (GET / -> 0.7.0) + health before
    reporting success.

Pre-deploy gates (fail-closed):
  * worker.js must contain a VERSION = "x.y.z" stamp (extracted and
    verified against the deployed version after upload),
  * node --check worker.js must pass,
  * node --test on the three test files must pass.
"""
import hashlib
import json
import os
import re
import subprocess
import sys
import urllib.request

sys.path.insert(0, "/opt/hatch/skills/skill-creator/bin")
from dynamic_credentials import add_surrogate_to_request, read_json_response

API = "https://api.cloudflare.com/client/v4"
ACCOUNT = "243aabef9144fd3b8dd334cb19319e75"
SCRIPT = "lucidota-exchange"
DB_ID = "0ea89801-9b0a-4efa-9d2f-170d7d285e69"
EXCHANGE = "https://lucidota-exchange.cutecity.workers.dev"
HERE = os.path.join(os.path.expanduser("~"), "workspace/exchange")


def req(method, path, data=None, headers=None):
    r = urllib.request.Request(API + path, data=data, method=method)
    for k, v in (headers or {}).items():
        r.add_header(k, v)
    add_surrogate_to_request(r, "custom.cloudflare",
                             allowed_hosts=["api.cloudflare.com"])
    with urllib.request.urlopen(r, timeout=120) as resp:
        return read_json_response(resp)


def get_live(path, timeout=30):
    # Browser UA: the edge 403s Python's default urllib UA on this host.
    req = urllib.request.Request(
        EXCHANGE + path,
        headers={"User-Agent": "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def main():
    worker = open(os.path.join(HERE, "worker.js"), "rb").read()
    envelope = open(os.path.join(HERE, "envelope.js"), "rb").read()

    # Gate 1: version stamp (extracted, then verified live after upload).
    m = re.search(rb'VERSION = "([0-9]+\.[0-9]+\.[0-9]+)"', worker)
    if not m:
        sys.exit("ABORT: worker.js has no VERSION = \"x.y.z\" stamp")
    version = m.group(1).decode()
    # Gate 2: syntax.
    subprocess.run(["node", "--check", os.path.join(HERE, "worker.js")],
                   check=True)
    # Gate 3: test suite.
    t = subprocess.run(["node", "--test",
                        os.path.join(HERE, "tests/envelope-vectors.test.mjs"),
                        os.path.join(HERE, "tests/hardening.test.mjs"),
                        os.path.join(HERE, "tests/ingest-envelope.test.mjs")],
                       capture_output=True, text=True)
    if t.returncode != 0:
        print(t.stdout[-2000:], file=sys.stderr)
        print(t.stderr[-2000:], file=sys.stderr)
        sys.exit("ABORT: node tests failed")
    print(f"gates green: version {version}, syntax ok, 3/3 test files pass")

    # Schedules before (must be unchanged after — we never touch them).
    sched_before = req("GET", f"/accounts/{ACCOUNT}/workers/scripts/{SCRIPT}/schedules")
    print("schedules before:", json.dumps(sched_before.get("result"))[:300])

    meta = json.dumps({
        "main_module": "worker.js",
        "compatibility_date": "2026-10-01",
        "bindings": [{"type": "d1", "name": "EXCHANGE_DB",
                      "database_id": DB_ID}],
    }).encode()
    bound = "----cfboundary070"
    parts = [
        b"--" + bound.encode() + b'\r\nContent-Disposition: form-data; name="metadata"\r\nContent-Type: application/json\r\n\r\n' + meta + b"\r\n",
        b"--" + bound.encode() + b'\r\nContent-Disposition: form-data; name="worker.js"; filename="worker.js"\r\nContent-Type: application/javascript+module\r\n\r\n' + worker + b"\r\n",
        b"--" + bound.encode() + b'\r\nContent-Disposition: form-data; name="envelope.js"; filename="envelope.js"\r\nContent-Type: application/javascript+module\r\n\r\n' + envelope + b"\r\n",
        b"--" + bound.encode() + b"--\r\n",
    ]
    out = req("PUT", f"/accounts/{ACCOUNT}/workers/scripts/{SCRIPT}",
              data=b"".join(parts),
              headers={"Content-Type": f"multipart/form-data; boundary={bound}"})
    print("deploy ok:", out.get("success"))
    if not out.get("success"):
        print(json.dumps(out.get("errors", []))[:800], file=sys.stderr)
        sys.exit(1)

    # Schedules after — prove we did not touch them.
    sched_after = req("GET", f"/accounts/{ACCOUNT}/workers/scripts/{SCRIPT}/schedules")
    print("schedules after: ", json.dumps(sched_after.get("result"))[:300])
    if sched_before.get("result") != sched_after.get("result"):
        sys.exit("ABORT-REPORT: schedules CHANGED during deploy — investigate before proceeding")

    # Verify the deployed version + health (migrate() runs on first hit).
    root = get_live("/")
    print("GET /:", json.dumps(root)[:400])
    if root.get("version") != version:
        sys.exit(f"ABORT-REPORT: deployed version is {root.get('version')}, not {version}")
    health = get_live("/v1/health")
    print("GET /v1/health:", json.dumps(health))
    if not health.get("ok"):
        sys.exit("ABORT-REPORT: /v1/health not ok after deploy")

    sha = hashlib.sha256(worker).hexdigest()
    print(f"DEPLOYED {version} sha256={sha} worker={len(worker)}B envelope={len(envelope)}B")
    print("schedules unchanged; secrets untouched; next: register-capabilities.py")


if __name__ == "__main__":
    main()
