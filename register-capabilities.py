#!/usr/bin/env python3
"""Register ALL enumerated capabilities on the Exchange (v0.7.0).

Reads capability-enumeration-2026-11-20.json (live /models probes) and:
  1. Inserts missing provider rows (cohere, friendli, zen).
  2. Updates nim-nvidia config: dead openai/gpt-oss-20b -> live
     google/gemma-4-31b-it, adds key_id + model_exact.
  3. Inserts 10 new inference-key resources (DISCOVERED leads — the
     gauntlet promotes them; registration is not capacity).
  4. Inserts resource_capabilities rows (inference + embedding where the
     key lists embedding models; "other" models are listed but have no
     execution path, so no capability row).
  5. Inserts ALL model_capabilities rows (656: every listed model, every
     cost class — paid rows are listed facts; the COST gate blocks their
     execution at $0).
  6. Seeds key_config (11 keys, failover order, all enabled, no
     allowlist/cap — knobs start open, Zack tightens via /v1/config).
  7. Writes evidence_log rows for the registration itself.

Idempotent (INSERT OR IGNORE / OR REPLACE / deterministic UPDATE).
Writes statements to register-batch.jsonl, executes via d1q-full.py
--batch-file (50/chunk), then verifies counts from D1.

Run AFTER deploying worker v0.7.0 (the tables are created by migrate()).
"""
import json
import os
import subprocess
import sys

HOME = os.path.expanduser("~")
HERE = os.path.join(HOME, "workspace/exchange")
ENUM = os.path.join(HERE, "capability-enumeration-2026-11-20.json")
BATCH = os.path.join(HERE, "register-batch.jsonl")

# key -> (resource_id, provider_id, name, cost_class, default_model, config)
# Default models are the $0 proof models; each is verified listed below.
KEYS = {
    "nim": ("nim-nvidia", "nvidia", "NVIDIA NIM (key)", "free",
            "google/gemma-4-31b-it", None),  # UPDATE, not insert
    "deepseek": ("deepseek", "deepseek", "DeepSeek (key)", "paid",
                 "deepseek-flash",
                 {"adapter": "claimed",
                  "baseUrl": "https://api.deepseek.com/v1",
                  "trust": "keyed", "via": "vm-claimant",
                  "credential_ref": "vault:custom.deepseek"}),
    "groq": ("groq", "groq", "Groq (key)", "free",
             "qwen/qwen3.8-27b",
             {"adapter": "claimed",
              "baseUrl": "https://api.groq.com/openai/v1",
              "trust": "keyed", "via": "vm-claimant",
              "credential_ref": "vault:custom.groq"}),
    "cerebras": ("cerebras", "cerebras", "Cerebras (key)", "free",
                 "qwen-3.8-27b",
                 {"adapter": "claimed",
                  "baseUrl": "https://api.cerebras.ai/v1",
                  "trust": "keyed", "via": "vm-claimant",
                  "credential_ref": "vault:custom.cerebras"}),
    "mistral": ("mistral", "mistral", "Mistral (key)", "unknown",
                "ministral-14b-latest",
                {"adapter": "claimed",
                 "baseUrl": "https://api.mistral.ai/v1",
                 "trust": "keyed", "via": "vm-claimant",
                 "credential_ref": "keyman:MISTRAL_KEY"}),
    "cohere": ("cohere", "cohere", "Cohere (key)", "free",
               "command-r7b-12-2024",
               {"adapter": "claimed",
                "baseUrl": "https://api.cohere.com",
                "trust": "keyed", "via": "vm-claimant",
                "credential_ref": "keyman:COHERE_KEY"}),
    "openrouter": ("openrouter", "openrouter", "OpenRouter (key)", "unknown",
                   "google/gemma-4-31b-it:free",
                   {"adapter": "claimed",
                    "baseUrl": "https://openrouter.ai/api/v1",
                    "trust": "keyed", "via": "vm-claimant",
                    "credential_ref": "keyman:OPENROUTER_KEY"}),
    "ai-router": ("ai-router", "ai-router", "ai-router failover pool", "free",
                  "gemini-2.0-flash",
                  {"adapter": "claimed",
                   "baseUrl": "https://ai-router.cutecity.workers.dev/v1",
                   "trust": "keyed", "via": "vm-claimant",
                   "credential_ref": "file:~/workspace/keys/ROUTER_KEY",
                   "model_exact": False,
                   "note": "failover pool: ignores the requested model, serves unpinned jobs only"}),
    "cloudflare-ai": ("cloudflare-ai", "cloudflare", "Cloudflare Workers AI (key)",
                      "free", "@cf/google/gemma-4-26b-a4b-it",
                      {"adapter": "claimed",
                       "baseUrl": "https://lucidota-ai.cutecity.workers.dev",
                       "trust": "keyed", "via": "vm-claimant",
                       "credential_ref": "keyman:CF_AI_PROXY_KEY"}),
    "friendli": ("friendli", "friendli", "Friendli (key)", "unknown",
                 "google/gemma-4-31B-it",
                 {"adapter": "claimed",
                  "baseUrl": "https://api.friendli.ai/serverless/v1",
                  "trust": "keyed", "via": "vm-claimant",
                  "credential_ref": "file:~/workspace/wreck/.env:FRIENDLI_API_KEY",
                  "note": "0 credits 2026-11-21 — unfunded, refuses until funded"}),
    "zen": ("zen", "zen", "Zen / OpenCode (key)", "unknown",
            "opencode/hy4-flash-free",
            {"adapter": "claimed",
             "trust": "keyed", "via": "opencode-subprocess",
             "credential_ref": "file:~/workspace/wreck/.env:OPENCODE_API_KEY"}),
}

FAILOVER_ORDER = {"nim": 10, "deepseek": 15, "cerebras": 20, "groq": 30,
                  "mistral": 40, "cohere": 50, "openrouter": 60, "zen": 70,
                  "cloudflare-ai": 80, "ai-router": 90, "friendli": 100}

NEW_PROVIDERS = [("cohere", "Cohere", "inference"),
                 ("friendli", "Friendli", "inference"),
                 ("zen", "Zen (OpenCode)", "inference")]

EVIDENCE_SOURCE = "live-/models 2026-11-21 (capability-enumeration-2026-11-20.json)"


def main():
    enum = json.load(open(ENUM))
    probed_at = enum["probed_at"]
    by_key = {r["key"]: r for r in enum["results"]}

    # 0. Verify every curated default model is actually listed for its key.
    # Never register a default the provider does not list.
    for key, (res_id, _prov, _name, _cc, default, _cfg) in KEYS.items():
        r = by_key.get(key)
        if key == "ai-router":
            continue  # failover pool: no model list by design
        listed = {m["model"] for m in (r or {}).get("models", [])}
        if default not in listed:
            sys.exit(f"ABORT: default model {default} NOT listed for key {key}")

    stmts = []

    def add(sql, params):
        stmts.append({"sql": sql, "params": params})

    # 1. Missing provider rows.
    for pid, name, kind in NEW_PROVIDERS:
        add("INSERT OR IGNORE INTO providers (id,name,kind,state) VALUES (?,?,?, 'UNKNOWN')",
            [pid, name, kind])

    # 2. nim-nvidia config fix (dead model -> live default + key binding).
    # Read current config from D1 first to preserve baseUrl/trust/via/refs.
    out = subprocess.run([sys.executable, os.path.join(HERE, "d1q-full.py"),
                          "SELECT config FROM resources WHERE id='nim-nvidia'"],
                         capture_output=True, text=True, check=True)
    cur = json.loads(out.stdout)[0]["results"][0]["config"]
    cfg = json.loads(cur)
    cfg["model"] = "google/gemma-4-31b-it"
    cfg["default_model"] = "google/gemma-4-31b-it"
    cfg["key_id"] = "nim"
    cfg["model_exact"] = True
    add("UPDATE resources SET config=? WHERE id='nim-nvidia'",
        [json.dumps(cfg, sort_keys=True)])
    add("INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)",
        ["nim-nvidia", "VERIFIED", "VERIFIED", "register-v0.7.0",
         "config: model openai/gpt-oss-20b (dead, 410) -> google/gemma-4-31b-it (live); key_id=nim, model_exact=true"])

    # 3. New resources + capabilities + evidence rows.
    for key, (res_id, prov, name, cc, default, cfg) in KEYS.items():
        if key == "nim":
            continue
        cfg = dict(cfg)
        cfg["key_id"] = key
        cfg.setdefault("model_exact", True)
        cfg["model"] = default
        cfg["default_model"] = default
        add("INSERT OR IGNORE INTO resources (id,provider_id,name,kind,state,cost_class,sovereignty,config,evidence_state,evidence_source,evidence_at,cost_basis,credential_state,claimant_trust_required) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            [res_id, prov, name, "inference", "UNKNOWN", cc, "external",
             json.dumps(cfg, sort_keys=True), "DISCOVERED", EVIDENCE_SOURCE,
             probed_at, "unknown", "unknown", "local"])
        caps = {"inference"}
        r = by_key.get(key)
        if r and any(m["capability"] == "embedding" for m in r["models"]):
            caps.add("embedding")
        for cap in sorted(caps):
            add("INSERT OR IGNORE INTO resource_capabilities (resource_id,capability,detail) VALUES (?,?,?)",
                [res_id, cap, json.dumps({"key": key})])
        add("INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)",
            [res_id, "nonexistent", "DISCOVERED", "register-v0.7.0",
             f"key {key} registered from live /models ({len(r['models']) if r else 0} models listed); DISCOVERED lead — gauntlet promotes"])

    # nim-nvidia also serves embeddings (8 models) — add the capability row.
    add("INSERT OR IGNORE INTO resource_capabilities (resource_id,capability,detail) VALUES (?,?,?)",
        ["nim-nvidia", "embedding", json.dumps({"key": "nim"})])

    # 4. Every listed model row (all capabilities, all cost classes).
    n_models = 0
    for key, (res_id, _prov, _name, _cc, _default, _cfg) in KEYS.items():
        r = by_key.get(key)
        if not r:
            continue
        for m in r["models"]:
            add("INSERT OR REPLACE INTO model_capabilities (resource_id,model,capability,cost,source,listed_at) VALUES (?,?,?,?,?,?)",
                [res_id, m["model"], m["capability"], m["cost"],
                 "live-/models 2026-11-21", probed_at])
            n_models += 1

    # 5. key_config seeds.
    for key in KEYS:
        add("INSERT OR REPLACE INTO key_config (key_id,enabled,allowlist,cost_cap,failover_order,updated_at,updated_by) VALUES (?,?,?,?,?,?,?)",
            [key, 1, None, None, FAILOVER_ORDER[key], probed_at, "register-v0.7.0"])

    with open(BATCH, "w") as f:
        for s in stmts:
            f.write(json.dumps(s) + "\n")
    print(f"statements: {len(stmts)} ({n_models} model rows)")
    print(f"wrote {BATCH}")

    # Execute via d1q-full.py --batch-file.
    out = subprocess.run([sys.executable, os.path.join(HERE, "d1q-full.py"),
                          "--batch-file", BATCH],
                         capture_output=True, text=True)
    print(out.stdout, end="")
    if out.returncode != 0:
        print(out.stderr, file=sys.stderr)
        sys.exit(f"batch failed rc={out.returncode}")

    # Verify counts from D1.
    def q(sql):
        out = subprocess.run([sys.executable, os.path.join(HERE, "d1q-full.py"), sql],
                             capture_output=True, text=True, check=True)
        return json.loads(out.stdout)[0]["results"]
    print("resources:", q("SELECT COUNT(*) c FROM resources")[0]["c"])
    print("model_capabilities:", q("SELECT COUNT(*) c FROM model_capabilities")[0]["c"])
    print("key_config:", q("SELECT COUNT(*) c FROM key_config")[0]["c"])
    print("per-resource model rows:")
    for row in q("SELECT resource_id, COUNT(*) c FROM model_capabilities GROUP BY resource_id ORDER BY resource_id"):
        print(f"  {row['resource_id']:16s} {row['c']}")
    print("per-resource state:")
    for row in q("SELECT id, state, evidence_state FROM resources WHERE id IN ('nim-nvidia','deepseek','groq','cerebras','mistral','cohere','openrouter','ai-router','cloudflare-ai','friendli','zen') ORDER BY id"):
        print(f"  {row['id']:16s} {row['state']:10s} {row['evidence_state']}")


if __name__ == "__main__":
    main()
