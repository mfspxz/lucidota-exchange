#!/usr/bin/env python3
"""exch-029: D1 mirror refresh (post-freeze). Phase 3 revision (2026-11-20).

Re-snapshots live D1 `exchange` to ~/workspace/exchange/d1-mirror.db.

Method: schema + paginated SELECT * per table through the Cloudflare D1
HTTP API (same surface as cf-d1.py). Tables are DISCOVERED live from
sqlite_master; tables are created from their real DDL (types preserved).

Resumable: progress is kept in d1-mirror.db.new + d1-mirror.progress.json.
A run interrupted mid-way (surrogate-credential socket timeouts killed two
Phase 3 runs) resumes by skipping tables already fetched; pass --fresh to
restart from scratch.

Drift: live D1 `exchange` is STILL MUTATING (health_observations grew
31,969 -> 31,981 across the Phase 3 runs; jobs/executions/leases also
ticked). A per-table count taken before pagination can disagree with the
count after it by a row or two. Each table is re-fetched up to 3 times on
mismatch; a table still mismatched after that is marked DRIFT (both counts
recorded in the stamp) and the refresh still completes — a snapshot of a
mutating DB cannot be lockstep-exact, and failing the whole refresh over
one row on a 32k-row table would leave the mirror stale forever.

Phase 3 fixes vs the Oct 7 version (original preserved at
refresh-mirror.py.pre-phase3-20261120.bak):
  1. Live table discovery (was: hardcoded 10 tables — missed 8 live tables).
  2. Real DDL from sqlite_master (was: TEXT-everything, inferred from the
     first row — crashed on empty tables).
  3. Removed the pragma_table_info call (D1 rejects PRAGMA via this API).
  4. Retry with backoff on transient HTTP 401/429/5xx AND on local
     surrogate-credential socket timeouts (both observed 2026-11-20).
  5. Stamp file written via temp + os.replace (was: open(w) truncate).
  6. Import path updated to /opt/muse/skills/custom-connector-setup/bin
     (old /opt/hatch/skills/skill-creator/bin kept as fallback).
  7. Resumable progress + drift tolerance (this revision).

2026-11-25 treatment (EXC-091 mirror-signature class): the refresh now
re-signs d1-mirror.db and d1-mirror.snapshot.json via `pl wsign` as its
final step. The .sig convention is live (DEPLOY-PROCEDURE-D1-SNAPSHOT.md
step 3 treats re-signing a stale mirror .sig as routine; EXC-100 did it
by hand after its refresh). Before this fix the script contained no
signing step at all, so every refresh stranded the mirror's .sig
unverifiable by construction. A sign failure now raises (nonzero exit)
instead of leaving a silently stale .sig.

Done = every table fetched and verified (OK or explicitly DRIFT-marked),
mirror replaced, snapshot timestamp updated, both artifacts re-signed.
"""
import hashlib
import json
import os
import sqlite3
import subprocess
import sys
import time
import urllib.error
import urllib.request

for _p in ("/opt/muse/skills/custom-connector-setup/bin",
           "/opt/hatch/skills/skill-creator/bin"):
    if os.path.exists(os.path.join(_p, "dynamic_credentials.py")):
        sys.path.insert(0, _p)
        break
from dynamic_credentials import add_surrogate_to_request, ensure_allowed_url, read_json_response

API = "https://api.cloudflare.com/client/v4"
HOSTS = ["api.cloudflare.com"]
ACCOUNT = "243aabef9144fd3b8dd334cb19319e75"
DB_UUID = "0ea89801-9b0a-4efa-9d2f-170d7d285e69"
MIRROR = os.path.expanduser("~/workspace/exchange/d1-mirror.db")
STAMP = os.path.expanduser("~/workspace/exchange/d1-mirror.snapshot.json")
PL = os.path.expanduser("~/workspace/proof-ledger/pl")
TMP = MIRROR + ".new"
PROGRESS = os.path.expanduser("~/workspace/exchange/d1-mirror.progress.json")
FREEZE_END = 1791417600  # 2026-10-08 00:00 UTC
PAGE = 500
MAX_REFETCH = 3


def api(method, path, body=None):
    url = API + path
    ensure_allowed_url(url, HOSTS)
    data = json.dumps(body).encode() if body is not None else None
    last_err = None
    for attempt in range(8):
        req = urllib.request.Request(url, data=data, method=method,
                                     headers={"Content-Type": "application/json"})
        try:
            add_surrogate_to_request(req, "custom.cloudflare", allowed_hosts=HOSTS)
            with urllib.request.urlopen(req, timeout=60) as resp:
                return read_json_response(resp)
        except urllib.error.HTTPError as e:
            last_err = e
            if e.code in (401, 403, 429) or e.code >= 500:
                time.sleep(min(60, 2 * (2 ** attempt)))
                continue
            raise
        except (TimeoutError, urllib.error.URLError, ConnectionError) as e:
            # The local surrogate-credential unix socket intermittently
            # times out mid-run (2026-11-20, outages >2 min observed).
            last_err = e
            time.sleep(min(60, 2 * (2 ** attempt)))
            continue
    raise last_err


def d1_query(sql, params=None):
    r = api("POST", f"/accounts/{ACCOUNT}/d1/database/{DB_UUID}/query",
            {"sql": sql, "params": params or []})
    if not r.get("success"):
        raise RuntimeError("D1 query failed: " + json.dumps(r.get("errors"))[:300])
    return r["result"][0]["results"]


def fetch_table(g, tname, ddl):
    """(Re)fetch one table into the scratch DB. Returns (rows, live_count)."""
    g.execute(f'DROP TABLE IF EXISTS "{tname}"')
    g.execute(ddl)
    live_n = d1_query(f'SELECT COUNT(*) c FROM "{tname}"')[0]["c"]
    offset = 0
    cols = None
    while offset < live_n:
        page = d1_query(
            f'SELECT * FROM "{tname}" ORDER BY rowid LIMIT {PAGE} OFFSET {offset}')
        if not page:
            break
        if cols is None:
            cols = list(page[0].keys())
        g.executemany(
            f'INSERT INTO "{tname}" ({",".join(chr(34) + c + chr(34) for c in cols)}) '
            f'VALUES ({",".join("?" * len(cols))})',
            [[r[c] if not isinstance(r[c], (dict, list)) else json.dumps(r[c])
              for c in cols] for r in page])
        offset += len(page)
        if len(page) < PAGE:
            break
    g.commit()
    n = g.execute(f'SELECT COUNT(*) FROM "{tname}"').fetchone()[0]
    return n, live_n


def sign_artifact(path, desc):
    """Re-sign one refreshed artifact with the estate wsign tool — the
    same mechanism/identity as the documented manual routine
    (`pl wsign op=sign`, key d452a6b7bdefd1ac). Raises on failure so a
    refresh can never report success while stranding a stale .sig."""
    try:
        r = subprocess.run([PL, "wsign", "op=sign", f"file={path}", f"desc={desc}"],
                           capture_output=True, text=True)
    except OSError as e:
        raise RuntimeError(f"wsign sign could not run for {path}: {e}")
    if r.returncode != 0:
        raise RuntimeError(f"wsign sign failed for {path}: "
                           f"{(r.stdout + r.stderr).strip()[:300]}")
    print(f"signed: {path}", flush=True)


def save_progress(progress):
    tmp = PROGRESS + ".tmp"
    with open(tmp, "w") as f:
        f.write(json.dumps(progress, indent=1))
        f.write("\n")
    os.replace(tmp, PROGRESS)


def main():
    if time.time() < FREEZE_END:
        print("FROZEN: mirror refresh refused until 2026-10-08 00:00 UTC")
        return 2
    fresh = "--fresh" in sys.argv
    if fresh:
        for p in (TMP, PROGRESS):
            if os.path.exists(p):
                os.remove(p)
    tables = d1_query(
        "SELECT name, sql FROM sqlite_master WHERE type='table' "
        "AND name NOT LIKE 'sqlite_%' AND name != '_cf_KV' ORDER BY name")
    indexes = d1_query(
        "SELECT name, sql FROM sqlite_master WHERE type='index' "
        "AND sql IS NOT NULL ORDER BY name")
    ddl_by_name = {t["name"]: t["sql"] for t in tables}
    progress = {}
    if os.path.exists(PROGRESS):
        progress = json.load(open(PROGRESS))
        print(f"resuming: {len(progress)} tables already fetched", flush=True)
    g = sqlite3.connect(TMP)
    manifest = {}
    total = 0
    for t in tables:
        tname = t["name"]
        if tname in progress and not fresh:
            n = g.execute(f'SELECT COUNT(*) FROM "{tname}"').fetchone()[0]
            live_now = d1_query(f'SELECT COUNT(*) c FROM "{tname}"')[0]["c"]
            if n == progress[tname]["rows"] == live_now:
                manifest[tname] = progress[tname]
                total += n
                print(f"{tname}: {n} rows (resumed, live count still matches)", flush=True)
                continue
            print(f"{tname}: resumed {n} rows but live is now {live_now}; re-fetching",
                  flush=True)
        n, live_n = fetch_table(g, tname, t["sql"])
        attempts = 1
        while n != live_n and attempts < MAX_REFETCH:
            print(f"{tname}: {n} vs live {live_n} — re-fetch {attempts + 1}/{MAX_REFETCH}",
                  flush=True)
            time.sleep(2)
            n, live_n = fetch_table(g, tname, t["sql"])
            attempts += 1
        status = "OK" if n == live_n else "DRIFT"
        manifest[tname] = {"mirror": n, "live": live_n, "match": n == live_n,
                           "status": status, "fetches": attempts}
        progress[tname] = manifest[tname] | {"rows": n}
        save_progress(progress)
        total += n
        print(f"{tname}: mirror={n} live={live_n} {status}", flush=True)
    for ix in indexes:
        try:
            g.execute(ix["sql"])
        except sqlite3.Error as e:
            print(f"index {ix['name']}: skipped ({e})")
    g.commit()
    print("mirror integrity:", g.execute("PRAGMA integrity_check").fetchone()[0])
    g.close()
    drift = [t for t, v in manifest.items() if v["status"] == "DRIFT"]
    if drift:
        print(f"DRIFT tables (snapshot kept, counts recorded): {drift}")
    os.replace(TMP, MIRROR)
    if os.path.exists(PROGRESS):
        os.remove(PROGRESS)
    snap = {"snapshot_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "rows": total, "tables": manifest,
            "drift_tables": drift,
            "sha256": hashlib.sha256(open(MIRROR, "rb").read()).hexdigest(),
            "bytes": os.path.getsize(MIRROR)}
    tmpstamp = STAMP + ".tmp"
    with open(tmpstamp, "w") as f:
        f.write(json.dumps(snap, indent=1))
        f.write("\n")
    os.replace(tmpstamp, STAMP)
    sign_artifact(MIRROR, f"D1 mirror refresh {snap['snapshot_utc']}: "
                          f"{total} rows, {len(manifest)} tables, "
                          f"drift={len(drift)}")
    sign_artifact(STAMP, f"D1 mirror stamp {snap['snapshot_utc']}: "
                         f"sha256 + counts for d1-mirror.db")
    print(f"mirror refreshed: {total} rows, {len(manifest)} tables, "
          f"drift={len(drift)}, stamp {STAMP}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
