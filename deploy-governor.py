#!/usr/bin/env python3
"""Deploy the Exchange governor worker.js via the Cloudflare Workers API.

Uses the skill's dynamic credential (custom.cloudflare) — never touches raw keys.
Steps: upload script (ES module + D1 binding) then set cron triggers.
"""
import sys, json, uuid, urllib.request

sys.path.insert(0, "/opt/hatch/skills/skill-creator/bin")
from dynamic_credentials import add_surrogate_to_request, read_json_response

ACCOUNT_ID = "243aabef9144fd3b8dd334cb19319e75"
SCRIPT_NAME = "lucidota-exchange"
D1_ID = "0ea89801-9b0a-4efa-9d2f-170d7d285e69"
HOSTS = ["api.cloudflare.com"]

def api(method, path, body=None, headers=None):
    url = f"https://api.cloudflare.com/client/v4{path}"
    req = urllib.request.Request(url, data=body, method=method)
    add_surrogate_to_request(req, "custom.cloudflare", allowed_hosts=HOSTS)
    for k, v in (headers or {}).items():
        req.add_header(k, v)
    resp = urllib.request.urlopen(req, timeout=120)
    return read_json_response(resp)

def main():
    worker_src = open("/home/hatch/workspace/exchange/worker.js", "rb").read()
    print(f"worker.js: {len(worker_src)} bytes", flush=True)

    metadata = {
        "main_module": "worker.js",
        "compatibility_date": "2026-10-01",
        "bindings": [
            {"type": "d1", "name": "EXCHANGE_DB", "id": D1_ID},
        ],
    }
    boundary = "----exchangedeploy" + uuid.uuid4().hex
    parts = []
    parts.append(
        f"--{boundary}\r\n"
        'Content-Disposition: form-data; name="metadata"\r\n'
        "Content-Type: application/json\r\n\r\n"
        + json.dumps(metadata) + "\r\n"
    )
    parts.append(
        f"--{boundary}\r\n"
        'Content-Disposition: form-data; name="worker.js"; filename="worker.js"\r\n'
        "Content-Type: application/javascript+module\r\n\r\n"
    )
    body = "".join(parts).encode() + worker_src + f"\r\n--{boundary}--\r\n".encode()

    print("uploading script...", flush=True)
    r = api("PUT", f"/accounts/{ACCOUNT_ID}/workers/scripts/{SCRIPT_NAME}", body,
            {"Content-Type": f"multipart/form-data; boundary={boundary}"})
    if not r.get("success"):
        print("UPLOAD FAILED:", json.dumps(r)[:2000])
        sys.exit(1)
    print("upload ok:", json.dumps(r.get("result", {}))[:300], flush=True)

    print("setting cron triggers...", flush=True)
    r2 = api("PUT", f"/accounts/{ACCOUNT_ID}/workers/scripts/{SCRIPT_NAME}/schedules",
             json.dumps([{"cron": "*/5 * * * *"}]).encode(),
             {"Content-Type": "application/json"})
    if not r2.get("success"):
        print("SCHEDULE FAILED:", json.dumps(r2)[:2000])
        sys.exit(1)
    print("cron ok:", json.dumps(r2.get("result", {}))[:300], flush=True)
    print("DEPLOY COMPLETE")

if __name__ == "__main__":
    main()
