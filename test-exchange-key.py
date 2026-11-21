#!/usr/bin/env python3
"""Validate ~/.exchange_key against the Exchange governor (read-only).

Reads the key from ~/.exchange_key in-process; NEVER prints it.
Test endpoint: GET /v1/ballot-policy (governor-side config, read-only).
Control: same endpoint without auth (expect 401 fail-closed).
Prints only HTTP statuses + a safe body summary (body printed only if it
does not contain the key string).
"""
import sys
import urllib.request
import urllib.error
from pathlib import Path

GOV = "https://lucidota-exchange.cutecity.workers.dev"
KEY_PATH = Path.home() / ".exchange_key"
# Cloudflare edge returns 1010 (bad browser signature) for urllib's default
# UA, blocking the request before it reaches the governor. Send a browser UA
# so the governor itself evaluates the request.
UA = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36"


def get(path, headers=None):
    h = {"User-Agent": UA}
    h.update(headers or {})
    req = urllib.request.Request(GOV + path, headers=h)
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")
    except Exception as e:
        return None, "%s: %s" % (type(e).__name__, e)


def safe_body(body, key):
    if key and key in body:
        return "<BODY WITHHELD: contains key material>"
    return body[:500]


def main():
    key = KEY_PATH.read_text().strip()
    if not key:
        print("KEY FILE EMPTY")
        return 2
    print("key file: %s (%d chars after strip, mode %s)"
          % (KEY_PATH, len(key), oct(KEY_PATH.stat().st_mode & 0o777)))
    s0, b0 = get("/v1/ballot-policy")
    print("control (no auth): HTTP %s :: %s" % (s0, safe_body(b0, key)))
    s1, b1 = get("/v1/ballot-policy",
                 {"Authorization": "Bearer %s" % key})
    print("with key: HTTP %s :: %s" % (s1, safe_body(b1, key)))
    if "1010" in b0 or "1010" in b1:
        print("VERDICT: INCONCLUSIVE (Cloudflare edge 1010 blocked the "
              "request before the governor saw it)")
        return 3
    if s1 == 200 and s0 in (401, 403):
        print("VERDICT: KEY VALID (governor-admin auth works; "
              "endpoint fail-closed without it)")
        return 0
    if s1 in (401, 403):
        print("VERDICT: KEY REJECTED (401/403 - key does not authenticate)")
        return 1
    print("VERDICT: INCONCLUSIVE (unexpected status combination)")
    return 3


if __name__ == "__main__":
    sys.exit(main())
