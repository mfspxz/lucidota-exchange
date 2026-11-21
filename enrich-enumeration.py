#!/usr/bin/env python3
"""Enrich capability-enumeration-2026-11-20.json with per-model classification.

Re-fetches full /models payloads (ids + provider metadata) and classifies each
model into a capability + cost class, deterministically:

  capability: inference (chat-completions) | embedding | other (guard, TTS,
              transcribe, rerank, detector, translate, vision-embed, ...)
  cost:       free | paid | unknown  (from provider pricing where published;
              OpenRouter pricing.prompt==0 -> free; Zen -free suffix -> free;
              Friendli -> unfunded (0 credits on the account))

Also patches the three special records established by live probes after the
first enumeration run:
  ai-router:    failover pool, NO model listing (404), IGNORES the requested
                model (4 Gemini ids all served by nemotron-3.5-bolt-e4b).
                capability = inference-failover, model_exact=false.
  gemini:       dead-key (401: bound service account deleted/disabled).
  cloudflare-ai: chat gemma-4-26b verified 200; 5 embedding models serve
                (bge-base clean, other 4 partial-body); full chat catalog
                unlistable (CF API token lacks AI scope, 403).
"""
import json
import os
import re
import subprocess
import sys
import urllib.request
from importlib.machinery import SourceFileLoader

sys.path.insert(0, "/opt/hatch/skills/skill-creator/bin")
from dynamic_credentials import add_surrogate_to_request, read_json_response  # noqa: E402

HOME = os.path.expanduser("~")
ENUM = os.path.join(HOME, "workspace/exchange/capability-enumeration-2026-11-20.json")

NON_CHAT = re.compile(
    r"embed|rerank|guard|safety|moderation|detector|translate|transcribe|"
    r"whisper|tts|orpheus|clip|gliner|nemoretriever|nv-embed|calibration|"
    r"cosmos|ocr|audio|summarization", re.I)
EMBED = re.compile(r"embed|bge", re.I)


def classify(model_id):
    if EMBED.search(model_id):
        return "embedding"
    if NON_CHAT.search(model_id):
        return "other"
    return "inference"


def keyman(name):
    return subprocess.run([os.path.join(HOME, "workspace/bin/keyman.py"), "get", name],
                          capture_output=True, text=True, check=True,
                          timeout=30).stdout.strip()


def get_json(url, headers=None, timeout=30):
    req = urllib.request.Request(url, method="GET", headers=headers or {})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def get_surrogate(url, cred, host, timeout=30):
    req = urllib.request.Request(url, method="GET")
    add_surrogate_to_request(req, cred, allowed_hosts=[host])
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return read_json_response(r)


def load_mod(name, path):
    return SourceFileLoader(name, os.path.expanduser(path)).load_module()


def main():
    doc = json.load(open(ENUM))
    by_key = {r["key"]: r for r in doc["results"]}

    # Full /models payloads (metadata for cost classification).
    full = {}
    nim = load_mod("nimcli", "~/workspace/skills/nvidia-nim/bin/nvidia-nim")
    full["nim"] = nim.api("GET", "/models").get("data", [])
    full["deepseek"] = get_surrogate("https://api.deepseek.com/v1/models",
                                     "custom.deepseek", "api.deepseek.com").get("data", [])
    groq = load_mod("groqcli", "~/workspace/skills/groq/bin/groq.py")
    full["groq"] = groq._request("GET", "/models").get("data", [])
    cere = load_mod("cerecli", "~/workspace/skills/cerebras/bin/cerebras.py")
    full["cerebras"] = cere._request("GET", "/models").get("data", [])
    full["mistral"] = get_json("https://api.mistral.ai/v1/models",
                               {"Authorization": "Bearer " + keyman("MISTRAL_KEY")}).get("data", [])
    full["cohere"] = get_json("https://api.cohere.com/v1/models",
                              {"Authorization": "Bearer " + keyman("COHERE_KEY")}).get("models", [])
    orc = load_mod("orcli", "~/workspace/skills/openrouter/bin/openrouter.py")
    full["openrouter"] = orc.call("GET", "/models").get("data", [])
    fenv = {}
    for line in open(os.path.join(HOME, "workspace/wreck/.env")):
        if "=" in line:
            k, v = line.strip().split("=", 1)
            fenv[k] = v.strip('"').strip("'")
    full["friendli"] = get_json("https://api.friendli.ai/serverless/v1/models",
                                {"Authorization": "Bearer " + fenv["FRIENDLI_API_KEY"]}).get("data", [])

    def cost_of(key, model_id, meta):
        if key == "friendli":
            return "unfunded"
        if key == "openrouter":
            pr = (meta or {}).get("pricing") or {}
            try:
                return "free" if float(pr.get("prompt", "1")) == 0 else "paid"
            except (TypeError, ValueError):
                return "unknown"
        if key == "zen":
            return "free" if model_id.endswith("-free") else "paid"
        if key in ("nim", "groq", "cerebras"):
            return "free"  # free-tier accounts; $0 executions measured at proof
        if key in ("deepseek", "mistral", "cohere"):
            return "unknown"  # paid accounts possible; spend gate decides at execution
        return "unknown"

    for key, items in full.items():
        rec = by_key[key]
        enriched = []
        for m in items:
            mid = m.get("id") or m.get("name")
            if not mid:
                continue
            enriched.append({"model": mid, "capability": classify(mid),
                             "cost": cost_of(key, mid, m)})
        # Keep models the first run saw but this fetch missed (none expected).
        seen = {e["model"] for e in enriched}
        for mid in rec["models"]:
            if mid not in seen:
                enriched.append({"model": mid, "capability": classify(mid),
                                 "cost": cost_of(key, mid, None),
                                 "note": "listed in run 1, absent in run 2"})
        rec["models"] = sorted(enriched, key=lambda e: e["model"])
        rec["count"] = len(enriched)
        rec["inference_count"] = sum(1 for e in enriched if e["capability"] == "inference")
        rec["embedding_count"] = sum(1 for e in enriched if e["capability"] == "embedding")
        rec["other_count"] = sum(1 for e in enriched if e["capability"] == "other")
        rec["free_count"] = sum(1 for e in enriched if e["cost"] == "free")

    # Zen: id-only list from run 1 (opencode models has no metadata).
    z = by_key["zen"]
    z["models"] = sorted(
        [{"model": m, "capability": classify(m), "cost": cost_of("zen", m, None)}
         for m in z["models"]], key=lambda e: e["model"])
    z["inference_count"] = sum(1 for e in z["models"] if e["capability"] == "inference")
    z["embedding_count"] = sum(1 for e in z["models"] if e["capability"] == "embedding")
    z["other_count"] = sum(1 for e in z["models"] if e["capability"] == "other")
    z["free_count"] = sum(1 for e in z["models"] if e["cost"] == "free")

    # Special records (live-probed, see module docstring).
    by_key["ai-router"] = {
        "key": "ai-router", "status": "failover-pool", "models": [],
        "count": 0, "inference_count": 0, "embedding_count": 0, "other_count": 0,
        "free_count": 0,
        "error": "/v1/models 404 (router_error); chat probes 2026-11-21 00:4xZ: "
                 "gemini-2.0-flash, gemini-2.5-flash, gemini-3.8-flash, "
                 "gemini-3.5-flash-lite ALL served by nvidia/nemotron-3.5-bolt-e4b "
                 "(200, content ok). The router IGNORES the requested model and "
                 "fails over silently (Gemini backend dead: GEMINI_KEY service "
                 "account deleted/disabled). Capability: inference-failover, "
                 "model_exact=false — jobs WITHOUT a pinned model only.",
        "capability_offered": "inference-failover",
        "model_exact": False,
        "served_by_now": "nvidia/nemotron-3.5-bolt-e4b",
    }
    by_key["gemini"] = {
        "key": "gemini", "status": "dead-key", "models": [], "count": 0,
        "inference_count": 0, "embedding_count": 0, "other_count": 0, "free_count": 0,
        "error": "GET /v1beta/models -> 401: 'The bound service account is "
                 "deleted or disabled.' GEMINI_KEY is dead for direct Gemini API. "
                 "Gemini reachability is via ai-router only, and the router's "
                 "Gemini backend is the same dead key (it fails over to NIM).",
    }
    by_key["cloudflare-ai"] = {
        "key": "cloudflare-ai", "status": "ok-partial", "count": 6,
        "inference_count": 1, "embedding_count": 5, "other_count": 0, "free_count": 6,
        "models": [
            {"model": "@cf/google/gemma-4-26b-a4b-it", "capability": "inference",
             "cost": "free", "note": "chat probe 200, 2026-11-21 00:4xZ"},
            {"model": "@cf/baai/bge-small-en-v1.5", "capability": "embedding",
             "cost": "free", "note": "partial-body (bytes flowed, transport truncated)"},
            {"model": "@cf/baai/bge-base-en-v1.5", "capability": "embedding",
             "cost": "free", "note": "clean 200, dims=768"},
            {"model": "@cf/baai/bge-large-en-v1.5", "capability": "embedding",
             "cost": "free", "note": "partial-body (bytes flowed, transport truncated)"},
            {"model": "@cf/baai/bge-m3", "capability": "embedding",
             "cost": "free", "note": "partial-body (bytes flowed, transport truncated)"},
            {"model": "@cf/qwen/qwen3-embedding-0.6b", "capability": "embedding",
             "cost": "free", "note": "partial-body (bytes flowed, transport truncated)"},
        ],
        "error": "Full Workers AI chat catalog UNLISTABLE with available "
                 "credentials: CF API GET /ai/models/search -> 403 (token lacks "
                 "AI scope; broadening scopes needs Zack). The lucidota-ai proxy "
                 "passes any model id through to env.AI.run, so catalog models "
                 "can be added by name later. Embedding allowlist is explicit in "
                 "the proxy source (5 models, all probed).",
    }
    for k in ("exaone", "inception"):
        by_key[k].update({"inference_count": 0, "embedding_count": 0,
                          "other_count": 0, "free_count": 0})

    doc["results"] = [by_key[k] for k in
                      ["nim", "deepseek", "groq", "cerebras", "mistral", "cohere",
                       "openrouter", "ai-router", "gemini", "cloudflare-ai",
                       "friendli", "zen", "exaone", "inception"]]
    doc["total_models"] = sum(r["count"] for r in doc["results"])
    doc["total_inference"] = sum(r.get("inference_count", 0) for r in doc["results"])
    doc["total_free_inference"] = sum(
        1 for r in doc["results"] for m in r.get("models", [])
        if isinstance(m, dict) and m.get("capability") == "inference"
        and m.get("cost") == "free")
    doc["keys_ok"] = sum(1 for r in doc["results"] if r["status"] in ("ok", "ok-partial", "failover-pool"))
    with open(ENUM, "w") as f:
        json.dump(doc, f, indent=1)
    for r in doc["results"]:
        print(f"{r['key']:14s} {r['status']:13s} {r['count']:4d} total | "
              f"inf {r.get('inference_count', 0):4d} emb {r.get('embedding_count', 0):3d} "
              f"other {r.get('other_count', 0):3d} free {r.get('free_count', 0):4d}")
    print(f"TOTAL {doc['total_models']} models, {doc['total_inference']} inference, "
          f"{doc['total_free_inference']} free inference, {doc['keys_ok']} live keys")


if __name__ == "__main__":
    main()
