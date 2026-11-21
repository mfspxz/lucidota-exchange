#!/usr/bin/env python3
"""Deploy a Rust/WASM Cloudflare Worker via the v4 API.

Usage: deploy-wasm.py --script <name> --js <worker.js> --wasm <worker.wasm>
       [--account <id>] [--d1 DATABASE_ID:binding ...] [--kv NAMESPACE_ID:binding ...]
       [--compat-date YYYY-MM-DD]

The JS file must import the WASM as: import wasm from './<wasm_filename>'
The metadata declares a wasm_module binding for the WASM part.
"""
import json
import os
import sys
import urllib.request
import uuid

sys.path.insert(0, "/opt/hatch/skills/skill-creator/bin")
from dynamic_credentials import (
    add_surrogate_to_request,
    ensure_allowed_url,
    read_json_response,
)

API = "https://api.cloudflare.com/client/v4"
HOSTS = ["api.cloudflare.com"]


def resolve_account(acct):
    if acct:
        return acct
    url = API + "/accounts?per_page=50"
    ensure_allowed_url(url, HOSTS)
    req = urllib.request.Request(url, method="GET")
    add_surrogate_to_request(req, "custom.cloudflare", allowed_hosts=HOSTS)
    with urllib.request.urlopen(req, timeout=30) as resp:
        r = read_json_response(resp)
    accts = r.get("result") or []
    if not accts:
        sys.exit("ERR: no Cloudflare accounts visible to this token")
    return accts[0]["id"]


def main():
    import argparse

    ap = argparse.ArgumentParser()
    ap.add_argument("--script", required=True)
    ap.add_argument("--js", required=True, help="Path to the worker JS file")
    ap.add_argument("--wasm", required=True, help="Path to the WASM binary")
    ap.add_argument("--account")
    ap.add_argument("--kv", nargs="*", default=[],
                    help="KV bindings as NAMESPACE_ID:binding_name")
    ap.add_argument("--d1", nargs="*", default=[],
                    help="D1 bindings as DATABASE_ID:binding_name")
    ap.add_argument("--compat-date", default="2026-10-01")
    args = ap.parse_args()

    acct = resolve_account(args.account)
    with open(args.js) as f:
        js_code = f.read()
    with open(args.wasm, "rb") as f:
        wasm_bytes = f.read()

    js_name = os.path.basename(args.js)
    wasm_name = os.path.basename(args.wasm)

    metadata = {
        "main_module": js_name,
        "compatibility_date": args.compat_date,
        "bindings": [
            {"type": "wasm_module", "name": "wasm", "part": wasm_name},
        ],
    }
    for item in args.kv:
        ns_id, _, name = item.partition(":")
        if not ns_id or not name:
            sys.exit(f"ERR: bad --kv value (want NAMESPACE_ID:binding_name): {item}")
        metadata["bindings"].append({"type": "kv_namespace", "namespace_id": ns_id, "name": name})
    for item in args.d1:
        db_id, _, name = item.partition(":")
        if not db_id or not name:
            sys.exit(f"ERR: bad --d1 value (want DATABASE_ID:binding_name): {item}")
        metadata["bindings"].append({"type": "d1", "name": name, "id": db_id})

    boundary = uuid.uuid4().hex
    # Build multipart manually (binary-safe).
    parts = []
    parts.append(
        f'--{boundary}\r\nContent-Disposition: form-data; name="metadata"\r\n'
        f"Content-Type: application/json\r\n\r\n{json.dumps(metadata)}\r\n".encode()
    )
    parts.append(
        f'--{boundary}\r\nContent-Disposition: form-data; name="{js_name}"; filename="{js_name}"\r\n'
        f"Content-Type: application/javascript+module\r\n\r\n".encode()
        + js_code.encode() + b"\r\n"
    )
    parts.append(
        f'--{boundary}\r\nContent-Disposition: form-data; name="{wasm_name}"; filename="{wasm_name}"\r\n'
        f"Content-Type: application/wasm\r\n\r\n".encode()
        + wasm_bytes + b"\r\n"
    )
    parts.append(f"--{boundary}--\r\n".encode())
    body = b"".join(parts)

    url = f"{API}/accounts/{acct}/workers/scripts/{args.script}"
    ensure_allowed_url(url, HOSTS)
    req = urllib.request.Request(
        url,
        data=body,
        method="PUT",
        headers={"Content-Type": f"multipart/form-data; boundary={boundary}"},
    )
    add_surrogate_to_request(req, "custom.cloudflare", allowed_hosts=HOSTS)
    with urllib.request.urlopen(req, timeout=120) as resp:
        r = read_json_response(resp)
    if not r.get("success"):
        sys.exit("ERR deploy: " + json.dumps(r.get("errors")))
    print(json.dumps({"success": True, "script": args.script,
                      "url": f"https://{args.script}.cutecity.workers.dev"}))


if __name__ == "__main__":
    main()
