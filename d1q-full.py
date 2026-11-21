#!/usr/bin/env python3
"""Full-output D1 query helper (cf.py d1q truncates at 2000 chars).

Usage:
  d1q-full.py "<sql>" [--db <id>] [--out <file>] [--batch-file <jsonl>]
  --batch-file: JSON lines of {"sql": ..., "params": [...]} executed via the
  D1 batch endpoint in chunks of 50. Prints per-chunk result counts.

Auth: custom.cloudflare vault surrogate (same as cf.py). Never prints secrets.
"""
import json
import sys
import urllib.error
import urllib.request

sys.path.insert(0, "/opt/muse/skills/custom-connector-setup/bin")
from dynamic_credentials import add_surrogate_to_request, read_json_response

API = "https://api.cloudflare.com/client/v4"
ACCOUNT = "243aabef9144fd3b8dd334cb19319e75"
DEFAULT_DB = "0ea89801-9b0a-4efa-9d2f-170d7d285e69"


def req(method, path, data=None):
    r = urllib.request.Request(API + path, data=data, method=method,
                               headers={"Content-Type": "application/json"})
    add_surrogate_to_request(r, "custom.cloudflare", allowed_hosts=["api.cloudflare.com"])
    try:
        with urllib.request.urlopen(r, timeout=120) as resp:
            return read_json_response(resp)
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace")[:800]
        raise SystemExit(f"CF API HTTP {e.code}: {body}")


def query(db_id, sql, params=None):
    body = json.dumps({"sql": sql, "params": params or []}).encode()
    out = req("POST", f"/accounts/{ACCOUNT}/d1/database/{db_id}/query", data=body)
    if not out.get("success"):
        raise SystemExit(f"D1 query failed: {json.dumps(out.get('errors', out))[:500]}")
    return out["result"]


def batch(db_id, stmts):
    """stmts: list of {"sql":..., "params":[...]}. Returns list of results.
    NOTE (2026-11-21): the D1 HTTP API has NO batch endpoint — /query
    takes a single object (array -> 400 code 7400) and /batch 404s. So
    statements run sequentially, one POST each, with progress every 50.
    Slower than a real batch, but correct; 706 statements ≈ 6-12 min."""
    out_all = []
    for i, s in enumerate(stmts):
        out_all.append(query(db_id, s["sql"], s.get("params") or [])[0])
        if (i + 1) % 50 == 0:
            print(f"  ... {i + 1}/{len(stmts)}", flush=True)
    return out_all


def main():
    args = sys.argv[1:]
    db_id = DEFAULT_DB
    out_path = None
    batch_file = None
    sql = None
    i = 0
    while i < len(args):
        if args[i] == "--db":
            db_id = args[i + 1]; i += 2
        elif args[i] == "--out":
            out_path = args[i + 1]; i += 2
        elif args[i] == "--batch-file":
            batch_file = args[i + 1]; i += 2
        else:
            sql = args[i]; i += 1
    if batch_file:
        stmts = [json.loads(line) for line in open(batch_file) if line.strip()]
        res = batch(db_id, stmts)
        print(f"batch ok: {len(res)} statements, "
              f"changes={sum((r.get('meta') or {}).get('changes', 0) for r in res)}")
        return
    if not sql:
        sys.exit("usage: d1q-full.py \"<sql>\" [--db <id>] [--out <file>]")
    res = query(db_id, sql)
    text = json.dumps(res, indent=1)
    if out_path:
        open(out_path, "w").write(text)
        print(f"wrote {out_path} ({len(text)} bytes)")
    else:
        print(text)


if __name__ == "__main__":
    main()
