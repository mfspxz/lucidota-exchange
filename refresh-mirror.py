#!/usr/bin/env python3
"""exch-029: D1 mirror refresh (post-freeze).
Re-snapshots live D1 to ~/workspace/exchange/d1-mirror.db after the freeze
lifts (>= 2026-10-08 00:00 UTC). Refuses to run during the freeze.

Method: paginated SELECT * per table through the Cloudflare D1 HTTP API
(same surface as ~/workspace/skills/cloudflare/bin/cf-d1.py). Requires a
token with D1 read scope — fails loudly otherwise (the 2026-10-07 token
lacked it; the 04:17 UTC snapshot was taken with broader credentials).

Done = row counts match live AND snapshot timestamp updated.
"""
import json, os, sqlite3, sys, time, urllib.request

sys.path.insert(0, "/opt/hatch/skills/skill-creator/bin")
from dynamic_credentials import add_surrogate_to_request, ensure_allowed_url, read_json_response

API = "https://api.cloudflare.com/client/v4"
HOSTS = ["api.cloudflare.com"]
ACCOUNT = "243aabef9144fd3b8dd334cb19319e75"
DB_UUID = "0ea89801-9b0a-4efa-9d2f-170d7d285e69"
TABLES = ["jobs", "receipts", "ballots", "claims", "executions", "accounting",
          "resources", "workers", "leases", "quotas"]
MIRROR = os.path.expanduser("~/workspace/exchange/d1-mirror.db")
STAMP = os.path.expanduser("~/workspace/exchange/d1-mirror.snapshot.json")
FREEZE_END = 1791417600  # 2026-10-08 00:00 UTC

def api(method, path, body=None):
    url = API + path
    ensure_allowed_url(url, HOSTS)
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method,
                                 headers={"Content-Type": "application/json"})
    add_surrogate_to_request(req, "custom.cloudflare", allowed_hosts=HOSTS)
    with urllib.request.urlopen(req, timeout=60) as resp:
        return read_json_response(resp)

def d1_query(sql, params=None):
    r = api("POST", f"/accounts/{ACCOUNT}/d1/database/{DB_UUID}/query",
            {"sql": sql, "params": params or []})
    if not r.get("success"):
        raise RuntimeError("D1 query failed: " + json.dumps(r.get("errors"))[:300])
    return r["result"][0]["results"]

def main():
    if time.time() < FREEZE_END:
        print("FROZEN: mirror refresh refused until 2026-10-08 00:00 UTC"); return 2
    tmp = MIRROR + ".new"
    if os.path.exists(tmp): os.remove(tmp)
    g = sqlite3.connect(tmp)
    total = 0
    manifest = {}
    for t in TABLES:
        cols = d1_query(f"SELECT name FROM pragma_table_info('{t}')")
        # pragma via D1 doesn't work; use SELECT * LIMIT 0 column names from meta
        rows, offset, got_cols = [], 0, None
        while True:
            page = d1_query(f"SELECT * FROM {t} LIMIT 500 OFFSET {offset}")
            if got_cols is None and page:
                got_cols = list(page[0].keys())
                coldefs = ", ".join(f'"{c}" TEXT' for c in got_cols)
                g.execute(f'CREATE TABLE "{t}" ({coldefs})')
            if not page: break
            for row in page:
                g.execute(f'INSERT INTO "{t}" VALUES ({",".join("?"*len(got_cols))})',
                          [row[c] if not isinstance(row[c], (dict, list)) else json.dumps(row[c])
                           for c in got_cols])
            offset += len(page)
            if len(page) < 500: break
        n = g.execute(f'SELECT COUNT(*) FROM "{t}"').fetchone()[0]
        live_n = d1_query(f"SELECT COUNT(*) c FROM {t}")[0]["c"]
        manifest[t] = {"mirror": n, "live": live_n, "match": n == live_n}
        total += n
        print(f"{t}: mirror={n} live={live_n} {'OK' if n==live_n else 'MISMATCH'}")
    g.commit(); g.close()
    if not all(v["match"] for v in manifest.values()):
        print("MISMATCH: not replacing mirror"); return 1
    os.replace(tmp, MIRROR)
    snap = {"snapshot_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "rows": total, "tables": manifest}
    open(STAMP, "w").write(json.dumps(snap, indent=1))
    print(f"mirror refreshed: {total} rows, stamp {STAMP}")
    return 0

if __name__ == "__main__":
    sys.exit(main())
