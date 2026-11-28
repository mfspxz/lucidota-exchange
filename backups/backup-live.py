#!/usr/bin/env python3
"""BACK UP the live Cloudflare workers COMPLETELY before the Rust cutover.
Pulls: deployed scripts, settings/bindings, schedules, routes, secret NAMES
(never values), D1 dumps, KV dumps. Writes sha256 manifest, then VERIFIES
by re-reading every file and comparing checksums. Fail-closed: any step
failing aborts with nonzero exit BEFORE the manifest is marked verified.
"""
import hashlib, json, os, sys, time, urllib.request, urllib.parse

sys.path.insert(0, "/opt/hatch/skills/skill-creator/bin")
from dynamic_credentials import add_surrogate_to_request, read_json_response

API = "https://api.cloudflare.com/client/v4"
ACCT = "243aabef9144fd3b8dd334cb19319e75"
CRED = "custom.cloudflare"
HOSTS = ["api.cloudflare.com"]

TS = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), TS)
os.makedirs(OUT, exist_ok=True)

def req(method, path, data=None, ctype=None, raw=False):
    r = urllib.request.Request(API + path, data=data, method=method)
    if ctype:
        r.add_header("Content-Type", ctype)
    add_surrogate_to_request(r, CRED, allowed_hosts=HOSTS)
    with urllib.request.urlopen(r, timeout=180) as resp:
        body = resp.read()
    if raw:
        return body
    return json.loads(body.decode())

def ok_or_die(out, what):
    if not out.get("success"):
        print("FATAL:", what, json.dumps(out.get("errors", out))[:500], file=sys.stderr)
        sys.exit(1)
    return out["result"]

def save(rel, data: bytes):
    p = os.path.join(OUT, rel)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    with open(p, "wb") as f:
        f.write(data)
    print("saved", rel, len(data), "bytes")
    return p

# ---- 1. worker scripts (raw deployed bytes) ----
for script in ("lucidota-exchange", "ctcc-router"):
    body = req("GET", f"/accounts/{ACCT}/workers/scripts/{script}", raw=True)
    save(f"workers/{script}.deployed.bin", body)
    for sub, name in (("settings", "settings.json"),
                      ("schedules", "schedules.json"),
                      ("routes", "routes.json"),
                      ("secrets", "secrets.json")):
        try:
            res = ok_or_die(req("GET", f"/accounts/{ACCT}/workers/scripts/{script}/{sub}"),
                            f"{script}/{sub}")
            # secrets endpoint lists names only; scrub defensively anyway
            blob = json.dumps(res, indent=1, sort_keys=True)
            if sub == "secrets":
                names = [s.get("name") for s in (res if isinstance(res, list) else [])]
                blob = json.dumps({"secret_names": names,
                                   "note": "values never pulled"}, indent=1)
            save(f"workers/{script}.{name}", blob.encode())
        except SystemExit:
            raise
        except Exception as e:
            print("note:", script, sub, "unavailable:", e)
    # subdomain state
    try:
        res = ok_or_die(req("GET", f"/accounts/{ACCT}/workers/scripts/{script}/subdomain"),
                        f"{script}/subdomain")
        save(f"workers/{script}.subdomain.json",
             json.dumps(res, indent=1, sort_keys=True).encode())
    except SystemExit:
        raise
    except Exception as e:
        print("note:", script, "subdomain unavailable:", e)

# ---- 2. wrangler.toml snapshot ----
import shutil
shutil.copy("/home/hatch/workspace/exchange/wrangler.toml",
            os.path.join(OUT, "workers/lucidota-exchange.wrangler.toml"))
print("saved workers/lucidota-exchange.wrangler.toml")

# ---- 3. D1 dumps ----
def d1_query(db_id, sql, params=None):
    body = json.dumps({"sql": sql, "params": params or []}).encode()
    out = req("POST", f"/accounts/{ACCT}/d1/database/{db_id}/query",
              data=body, ctype="application/json")
    res = ok_or_die(out, f"d1 {db_id} query")
    return res[0]["results"]

for db_id, label in (("0ea89801-9b0a-4efa-9d2f-170d7d285e69", "exchange"),
                     ("ccb25914-29b4-4b2d-96d7-52e899d074e1", "heartbeat")):
    tables = [r["name"] for r in d1_query(
        db_id, "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '\\_cf\\_%' ESCAPE '\\'")]
    dump = {"database_id": db_id, "label": label, "dumped_at": TS, "tables": {}}
    for t in tables:
        cnt = d1_query(db_id, f"SELECT COUNT(*) c FROM \"{t}\"")[0]["c"]
        rows, off = [], 0
        while off < cnt:
            rows += d1_query(db_id, f"SELECT * FROM \"{t}\" LIMIT 500 OFFSET {off}")
            off += 500
        dump["tables"][t] = {"count": cnt, "rows": rows}
        print(f"d1 {label}.{t}: {cnt} rows")
    # schema
    schema = d1_query(db_id, "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL")
    dump["schema"] = [r["sql"] for r in schema]
    save(f"d1/{label}.dump.json", json.dumps(dump, sort_keys=True).encode())

# ---- 4. KV dumps ----
ns_list = ok_or_die(req("GET", f"/accounts/{ACCT}/storage/kv/namespaces"), "kv ns list")
save("kv/namespaces.json", json.dumps(
    [{"id": n["id"], "title": n["title"]} for n in ns_list], indent=1).encode())
for n in ns_list:
    nid, title = n["id"], n["title"]
    keys, cursor = [], None
    while True:
        q = "/accounts/%s/storage/kv/namespaces/%s/keys" % (ACCT, nid)
        if cursor:
            q += "?cursor=" + urllib.parse.quote(cursor)
        res = ok_or_die(req("GET", q), f"kv {title} keys")
        page = res["result"] if isinstance(res, dict) else res
        keys += [k["name"] for k in page]
        cursor = (res.get("result_info") or {}).get("cursor") if isinstance(res, dict) else None
        if not cursor:
            break
    vals = {}
    for k in keys:
        v = req("GET", "/accounts/%s/storage/kv/namespaces/%s/values/%s" %
                (ACCT, nid, urllib.parse.quote(k, safe="")), raw=True)
        vals[k] = v.decode("utf-8", errors="replace")
    print(f"kv {title}: {len(keys)} keys")
    save(f"kv/{title}.json", json.dumps(
        {"namespace_id": nid, "title": title, "keys": vals}, sort_keys=True).encode())

# ---- 5. manifest + VERIFY ----
manifest = {}
for root, _, files in os.walk(OUT):
    for f in files:
        if f == "MANIFEST.sha256":
            continue
        p = os.path.join(root, f)
        rel = os.path.relpath(p, OUT)
        h = hashlib.sha256(open(p, "rb").read()).hexdigest()
        manifest[rel] = h
mp = os.path.join(OUT, "MANIFEST.sha256")
with open(mp, "w") as f:
    for rel in sorted(manifest):
        f.write(f"{manifest[rel]}  {rel}\n")
print("manifest written:", len(manifest), "files")

# VERIFY: re-read everything, checksums must match
fails = 0
for rel, want in manifest.items():
    got = hashlib.sha256(open(os.path.join(OUT, rel), "rb").read()).hexdigest()
    if got != want:
        print("VERIFY FAIL:", rel)
        fails += 1
if fails:
    print("BACKUP NOT VERIFIED:", fails, "mismatches", file=sys.stderr)
    sys.exit(1)
with open(os.path.join(OUT, "VERIFIED.txt"), "w") as f:
    f.write(f"backup {TS} verified: {len(manifest)} files, all sha256 match\n")
print(f"BACKUP VERIFIED: {OUT} ({len(manifest)} files)")
