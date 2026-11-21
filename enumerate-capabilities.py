#!/usr/bin/env python3
"""Enumerate EVERY provider key's live capability set via /models probes.

Zack's order (2026-11-20 18:15 CST): ALL capabilities of EVERY key available
on the Exchange. This script is the ground truth: it probes each key's live
/models endpoint (or equivalent) and records exactly what the key can reach
RIGHT NOW. No static lists, no spec sheets.

Credential paths (estate surrogates only, never raw keys in output):
  nim, deepseek, groq, cerebras, cloudflare-ai: dynamic-credential surrogates
  mistral, cohere, openrouter, gemini, ai-router: keyman / key files
  friendli, zen: wreck/.env (transient in-process read, never printed)
  exaone, inception: NO KEY in the estate — recorded, not probed.

Output: capability-enumeration-2026-11-20.json (same dir) + compact summary.
Exit 0 even when individual probes fail — a failed probe is a recorded fact.
"""
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
from importlib.machinery import SourceFileLoader

sys.path.insert(0, "/opt/hatch/skills/skill-creator/bin")
from dynamic_credentials import (  # noqa: E402
    add_surrogate_to_request, read_json_response, DynamicCredentialError)

HOME = os.path.expanduser("~")
OUT = os.path.join(HOME, "workspace/exchange/capability-enumeration-2026-11-20.json")
CF_ACCOUNT = "243aabef9144fd3b8dd334cb19319e75"


def load_mod(name, path):
    return SourceFileLoader(name, os.path.expanduser(path)).load_module()


def keyman(name):
    out = subprocess.run([os.path.join(HOME, "workspace/bin/keyman.py"), "get", name],
                         capture_output=True, text=True, check=True, timeout=30)
    return out.stdout.strip()


def env_file(path):
    """Read a KEY=value env file. Returns dict. Values never leave this process."""
    d = {}
    with open(os.path.expanduser(path)) as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            k, v = line.split("=", 1)
            d[k.strip()] = v.strip().strip('"').strip("'")
    return d


def get_json(url, headers=None, timeout=30):
    req = urllib.request.Request(url, method="GET", headers=headers or {})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def get_surrogate(url, cred, host, entry="access_token", timeout=30):
    req = urllib.request.Request(url, method="GET")
    try:
        add_surrogate_to_request(req, cred, entry_name=entry, allowed_hosts=[host])
    except DynamicCredentialError:
        add_surrogate_to_request(req, cred, entry_name="acccess_token",
                                 allowed_hosts=[host])
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return read_json_response(r)


def probe(name, fn):
    t0 = time.time()
    try:
        models = fn()
        return {"key": name, "status": "ok", "models": sorted(set(models)),
                "count": len(set(models)),
                "ms": int((time.time() - t0) * 1000)}
    except urllib.error.HTTPError as e:
        try:
            detail = e.read().decode("utf-8", errors="replace")[:200]
        except Exception:
            detail = ""
        return {"key": name, "status": f"http_{e.code}", "models": [],
                "count": 0, "error": detail,
                "ms": int((time.time() - t0) * 1000)}
    except Exception as e:
        return {"key": name, "status": "error", "models": [], "count": 0,
                "error": f"{type(e).__name__}: {str(e)[:200]}",
                "ms": int((time.time() - t0) * 1000)}


def main():
    results = []
    ts = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())

    # 1. NVIDIA NIM (surrogate)
    nim = load_mod("nimcli", "~/workspace/skills/nvidia-nim/bin/nvidia-nim")
    results.append(probe("nim", lambda: [
        m["id"] for m in nim.api("GET", "/models").get("data", [])]))

    # 2. DeepSeek (surrogate)
    results.append(probe("deepseek", lambda: [
        m["id"] for m in get_surrogate(
            "https://api.deepseek.com/v1/models",
            "custom.deepseek", "api.deepseek.com").get("data", [])]))

    # 3. Groq (surrogate, module _request)
    groq = load_mod("groqcli", "~/workspace/skills/groq/bin/groq.py")
    results.append(probe("groq", lambda: [
        m["id"] for m in groq._request("GET", "/models").get("data", [])]))

    # 4. Cerebras (surrogate, module _request)
    cere = load_mod("cerecli", "~/workspace/skills/cerebras/bin/cerebras.py")
    results.append(probe("cerebras", lambda: [
        m["id"] for m in cere._request("GET", "/models").get("data", [])]))

    # 5. Mistral (keyman)
    results.append(probe("mistral", lambda: [
        m["id"] for m in get_json(
            "https://api.mistral.ai/v1/models",
            {"Authorization": "Bearer " + keyman("MISTRAL_KEY")}).get("data", [])]))

    # 6. Cohere (keyman) — /v1/models returns {models: [...]}
    results.append(probe("cohere", lambda: [
        m["name"] for m in get_json(
            "https://api.cohere.com/v1/models",
            {"Authorization": "Bearer " + keyman("COHERE_KEY")}).get("models", [])]))

    # 7. OpenRouter (keyman, module call)
    orc = load_mod("orcli", "~/workspace/skills/openrouter/bin/openrouter.py")
    results.append(probe("openrouter", lambda: [
        m["id"] for m in orc.call("GET", "/models").get("data", [])]))

    # 8. ai-router (ROUTER_KEY file) — OpenAI-compatible /v1/models
    rkey = open(os.path.join(HOME, "workspace/keys/ROUTER_KEY")).read().strip()
    results.append(probe("ai-router", lambda: [
        m["id"] for m in get_json(
            "https://ai-router.cutecity.workers.dev/v1/models",
            {"Authorization": "Bearer " + rkey}).get("data", [])]))

    # 9. Gemini direct (keyman GEMINI_KEY) — names are models/<id>
    results.append(probe("gemini", lambda: [
        m["name"].replace("models/", "") for m in get_json(
            "https://generativelanguage.googleapis.com/v1beta/models?pageSize=100",
            {"x-goog-api-key": keyman("GEMINI_KEY")}).get("models", [])]))

    # 10. Cloudflare Workers AI catalog (CF API surrogate)
    results.append(probe("cloudflare-ai", lambda: [
        m["id"] for m in get_surrogate(
            f"https://api.cloudflare.com/client/v4/accounts/{CF_ACCOUNT}/ai/models/search",
            "custom.cloudflare", "api.cloudflare.com").get("result", [])]))

    # 11. Friendli (wreck/.env, 0 credits expected)
    fenv = env_file("~/workspace/wreck/.env")
    results.append(probe("friendli", lambda: [
        m["id"] for m in get_json(
            "https://api.friendli.ai/serverless/v1/models",
            {"Authorization": "Bearer " + fenv["FRIENDLI_API_KEY"]}).get("data", [])]))

    # 12. Zen / OpenCode (opencode models via the binary; key from wreck/.env)
    def zen_models():
        env = dict(os.environ)
        env["OPENCODE_API_KEY"] = fenv["OPENCODE_API_KEY"]
        p = subprocess.run(
            [os.path.join(HOME, ".opencode/bin/opencode"), "models"],
            capture_output=True, text=True, timeout=60, stdin=subprocess.DEVNULL,
            env=env, cwd=os.path.join(HOME, "workspace/wreck"))
        if p.returncode != 0:
            raise RuntimeError(f"opencode models rc={p.returncode}: "
                               f"{(p.stderr or p.stdout)[:200]}")
        # Output is a table; zen models appear as opencode/<model>.
        out = []
        for line in (p.stdout or "").splitlines():
            for tok in line.replace("|", " ").split():
                if tok.startswith("opencode/"):
                    out.append(tok)
        if not out:
            raise RuntimeError("no opencode/ models in output: "
                               f"{p.stdout[:200]!r}")
        return out
    results.append(probe("zen", zen_models))

    # 13-14. No key in the estate — recorded, not probed.
    results.append({"key": "exaone", "status": "no-key", "models": [], "count": 0,
                    "error": "EXAONE_API_KEY never received; no account to be created"})
    results.append({"key": "inception", "status": "no-key", "models": [], "count": 0,
                    "error": "Mercury sign-ups blocked; no key in estate"})

    doc = {"probed_at": ts, "probe": "live /models per key, estate surrogates",
           "results": results,
           "total_models": sum(r["count"] for r in results),
           "keys_ok": sum(1 for r in results if r["status"] == "ok")}
    with open(OUT, "w") as f:
        json.dump(doc, f, indent=1)
    for r in results:
        extra = f" ({r['error'][:100]})" if r["status"] != "ok" else ""
        print(f"{r['key']:14s} {r['status']:10s} {r['count']:4d} models{extra}")
    print(f"TOTAL {doc['total_models']} models across {doc['keys_ok']} live keys")
    print("wrote", OUT)


if __name__ == "__main__":
    main()
