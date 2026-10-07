#!/usr/bin/env python3
"""Local skill discovery — the Exchange cannot see the VM's vault, so this
sidecar scans workspace skills for vault-connected API capabilities and
registers the missing ones as DISCOVERED provider candidates via /v1/discover.

A listing is never capacity: these enter as leads (usable=false) and earn
their tiers through the evidence ladder like everything else. Run weekly.
"""
import glob
import json
import os
import re
import urllib.request

EXCHANGE = "https://lucidota-exchange.cutecity.workers.dev"
SKILLS = os.path.expanduser("~/workspace/skills")

# Skill dirs already represented under a different provider id.
ALIASES = {"nvidia-nim": "nvidia"}

KEY = open(os.path.expanduser("~/.exchange_key")).read().strip()


def xchg(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        EXCHANGE + path, data=data, method=method,
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {KEY}",
                 "User-Agent": "lucidota-exchange-discovery/1.0"},
    )
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.loads(r.read())


def parse_skill(path):
    text = open(path).read()
    name = os.path.basename(os.path.dirname(path))
    m = re.search(r'^name:\s*"([^"]+)"', text, re.M)
    title = m.group(1) if m else name
    m = re.search(r'^description:\s*"([^"]+)"', text, re.M)
    desc = m.group(1) if m else ""
    creds = sorted(set(re.findall(r"custom\.([a-z0-9_-]+)", text)))
    hosts = sorted(set(re.findall(r"https?://([a-z0-9_.-]+)", text)))
    return {"dir": name, "title": title, "desc": desc, "creds": creds, "hosts": hosts}


def main():
    existing = set()
    try:
        res = xchg("GET", "/v1/resources")
        for r in res.get("resources", []):
            existing.add(r.get("provider"))
    except Exception as e:
        print(json.dumps({"error": f"list failed: {e}"[:120]}))
        return 1
    added = []
    for md in sorted(glob.glob(os.path.join(SKILLS, "*", "SKILL.md"))):
        s = parse_skill(md)
        if not s["creds"]:
            continue  # no vault credential → nothing to bridge
        alias = ALIASES.get(s["dir"], s["dir"])
        if alias in existing:
            continue
        notes = (f"Local skill '{s['title']}': {s['desc']} "
                 f"Vault credential(s): {', '.join('custom.' + c for c in s['creds'])}. "
                 f"Hosts: {', '.join(s['hosts'][:4]) or 'n/a'}.")
        try:
            r = xchg("POST", "/v1/discover", {
                "provider_id": alias, "provider_name": s["title"],
                "resource": False, "notes": notes,
                "evidence_source": "local-skill-scan",
            })
            if not r.get("error"):
                added.append(alias)
                existing.add(alias)
        except Exception as e:
            print(json.dumps({"warn": f"{alias}: {e}"[:120]}))
    print(json.dumps({"added": added, "scanned": len(glob.glob(os.path.join(SKILLS, '*', 'SKILL.md')))}))
    return 0


if __name__ == "__main__":
    main()
