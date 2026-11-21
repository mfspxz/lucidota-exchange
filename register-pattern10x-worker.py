#!/usr/bin/env python3
"""Register the PATTERN-10X programme worker on the Exchange governor.

Admin call: POST /v1/workers/register with the Bearer EXCHANGE_KEY
(read from ~/.exchange_key in-process; NEVER printed, logged, or passed
on a command line). Mints worker_id + worker_secret (shown once) and
stores them as two lines (worker_id, worker_secret) in
~/.exchange_worker_pattern10x, mode 600 — the established format read
by mint-claimant.py / nim-claimant.py.

Transport: curl via subprocess (urllib RemoteDisconnected 2/2 on this
path 2026-11-19; curl returned a clean 409 on the same endpoint). The
Bearer key travels in a 0600 curl config file built in-process, never
on a command line; the response (contains the worker secret) goes to a
temp file, is parsed in-process, and is deleted immediately.
"""
import json
import os
import subprocess
import sys
from pathlib import Path

GOV = "https://lucidota-exchange.cutecity.workers.dev"
KEY_PATH = Path.home() / ".exchange_key"
CRED_PATH = Path.home() / ".exchange_worker_pattern10x_02"
UA = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36"

WORKER_ID = "pattern10x_02"
TRUST_CLASS = "local"
NAME = "PATTERN-10X programme worker (W0)"
# NOTE: worker_id "pattern10x" was minted by a first attempt whose
# response was lost to a RemoteDisconnected — the governor registered
# it, but the secret is unknown to anyone. It is orphaned (harmless:
# nobody can authenticate as it). This is the usable second identity.


def main():
    key = KEY_PATH.read_text().strip()
    if not key:
        print("KEY FILE EMPTY")
        return 2
    if CRED_PATH.exists():
        print("CRED FILE ALREADY EXISTS: %s — refusing to overwrite "
              "(a registered worker's secret is shown once; re-register "
              "under a new worker_id if this file is lost)" % CRED_PATH)
        return 3
    body = {"worker_id": WORKER_ID, "trust_class": TRUST_CLASS, "name": NAME}
    cfg = Path("/tmp/exchg-reg.cfg")
    resp_file = Path("/tmp/exchg-reg.json")
    try:
        cfg.write_text(
            'header = "Authorization: Bearer %s"\n'
            'header = "User-Agent: %s"\n' % (key, UA))
        os.chmod(cfg, 0o600)
        proc = subprocess.run(
            ["curl", "-s", "-w", "\nHTTP:%{http_code}\n", "-K", str(cfg),
             "-H", "Content-Type: application/json", "-X", "POST",
             GOV + "/v1/workers/register",
             "-d", json.dumps(body), "-o", str(resp_file)],
            capture_output=True, text=True, timeout=60)
        out = proc.stdout or ""
        status = None
        for line in out.splitlines():
            if line.startswith("HTTP:"):
                try:
                    status = int(line.split(":", 1)[1])
                except ValueError:
                    pass
        raw = resp_file.read_text() if resp_file.exists() else ""
    except Exception as e:
        print("ERROR: %s: %s" % (type(e).__name__, e))
        return 1
    finally:
        for p in (cfg, resp_file):
            try:
                p.unlink()
            except OSError:
                pass
    try:
        resp = json.loads(raw)
    except Exception:
        print("HTTP %s :: non-JSON response (first 200 chars): %s" % (status, raw[:200]))
        return 1
    wid = resp.get("worker_id")
    wsec = resp.get("worker_secret")
    if status != 200 or not wid or not wsec:
        print("HTTP %s :: registration failed: %s" % (status, raw[:300]))
        return 1
    # Write creds: two lines, mode 600, no print of the secret.
    fd = os.open(CRED_PATH, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as f:
        f.write("%s\n%s\n" % (wid, wsec))
    st = CRED_PATH.stat()
    print("REGISTERED worker_id=%s trust_class=%s" % (wid, resp.get("trust_class")))
    print("stored: %s (mode %s, %d bytes, 2 lines)" % (CRED_PATH, oct(st.st_mode & 0o777), st.st_size))
    print("NOTE: secret shown once by the governor; stored only here (sha256 on the governor).")
    return 0


if __name__ == "__main__":
    sys.exit(main())
