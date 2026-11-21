#!/usr/bin/env python3
"""Generic multi-provider Exchange claimant — Worker Protocol v1 (v0.7.4).

v0.7.4: /v1/complete 409-replay is reported honestly (the first attempt
landed; the retry hit replay protection) instead of printing nulls.
v0.7.3: curl fallback when urllib truncates a read (IncompleteRead —
curl survives it 3/3 on the 14KB Cloudflare embedding response where
urllib truncated 3/3); NIM embeddings bypass the skill module for a
direct surrogate POST (the ~35KB 2048-dim response needs the fallback).
v0.7.2: claim RESUME (a claim response lost to transport is resumed, not
blind-retried — the blind retry orphaned 3 leases in proof attempt 3);
300s SIGALRM wall-clock guard on every execute (three hook workers wedged
~20 min at 0 CPU on 2026-11-21, cause unproven).
v0.7.1: provider POSTs retry 3x on transport errors only (IncompleteRead
on this egress path); Zen subprocess calls never retry.
v0.7.0: exact-model binding, no fallback, no silent substitution.

BORING BY DESIGN. This worker does exactly five things:
  PICKUP    -> POST /v1/claim (worker-scoped auth - never the Exchange key)
  VALIDATE  -> delegation fields present, lease not expired, MODEL BINDING
               exact (delegation model == job model, or refuse unexecuted)
  EXECUTE   -> exactly one provider call for the delegated resource's key
  CAPTURE   -> raw result, no interpretation
  REPORT    -> POST /v1/complete with exactly one terminal outcome + a
               lucidota-receipt/1 envelope sealed by the authority key

EXACT-MODEL RULE (v0.7.0, Zack 2026-11-20 — closes the 00:06 UTC Nov 21
mismatch: job asked openai/gpt-oss-20b, claimant executed
nvidia/nemotron-3-ultra-550b, still recorded ok):
  * The delegation binds the model. If the job pinned a model and the
    delegation names a different one, the claimant REFUSES WITHOUT
    EXECUTING (FAILED_PERMANENT / MODEL_MISMATCH). A disagreement is a
    governor bug; executing either model would be a substitution.
  * A pinned model NEVER executes on a failover-pool resource
    (ai-router, model_exact=false) — refuse without executing.
  * On 404/410 the model is GONE: FAILED_PERMANENT / NOT_FOUND, no
    fallback, no second call. The old phase3 fallback (gpt-oss-20b ->
    nemotron-3-ultra-550b) is DELETED — it was the mismatch mechanism.
  * model_actual is what the provider REPORTED (or the invocation-bound
    model where the provider echoes nothing, e.g. Cohere, Zen). The
    governor verifies exactness at completion (model_match=0 on an exact
    resource -> VERIFICATION_FAILURE); this worker never reclassifies.

PROVIDERS (dispatch by CLAIM_RESOURCE; credentials via estate surrogates,
keyman, or key files — never raw keys in output, never in argv):
  nim-nvidia    nvidia-nim skill module (surrogate)
  deepseek      surrogate POST api.deepseek.com/v1
  groq          groq skill module (surrogate)
  cerebras      cerebras skill module (surrogate)
  mistral       keyman MISTRAL_KEY -> api.mistral.ai/v1
  cohere        keyman COHERE_KEY -> api.cohere.com/v2 (own shapes)
  openrouter    keyman OPENROUTER_KEY -> openrouter.ai/api/v1 (inline
                urllib: the skill module's call() sys.exits on HTTPError)
  ai-router     ROUTER_KEY file -> ai-router.cutecity.workers.dev/v1
                (failover pool: serves unpinned jobs only, model_exact=false)
  cloudflare-ai keyman CF_AI_PROXY_KEY -> lucidota-ai proxy /v1/chat,
                /v1/embeddings (x-proxy-key)
  friendli      wreck/.env FRIENDLI_API_KEY -> api.friendli.ai/serverless/v1
                (0 credits expected: the governor refuses before claiming)
  zen           opencode run --model opencode/<m> subprocess, stdin DEVNULL,
                timeout 300, --auto (hangs silently otherwise — see AGENTS.md)

Capabilities: inference (chat) and embedding. Anything else is refused
without executing (FAILED_PERMANENT / BAD_REQUEST).

It never: retries, fails over to another provider or model, chooses
models, asserts cost, extends its lease, or decides what happens next.
The governor owns authority; this process owns execution.
"""
import hashlib
import http.client
import json
import os
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request
from importlib.machinery import SourceFileLoader

sys.path.insert(0, "/opt/hatch/skills/skill-creator/bin")
from dynamic_credentials import (  # noqa: E402
    add_surrogate_to_request, DynamicCredentialError)

EXCHANGE = "https://lucidota-exchange.cutecity.workers.dev"
RESOURCE = os.environ.get("CLAIM_RESOURCE", "nim-nvidia")
WORKER_ID_FILE = os.environ.get(
    "WORKER_CRED_FILE", os.path.expanduser("~/.exchange_worker_nim"))
DRAIN_CAP = int(os.environ.get("CLAIM_DRAIN_CAP", "25"))
HOME = os.path.expanduser("~")
BROWSER_UA = ("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 "
              "(KHTML, like Gecko) Chrome/120.0 Safari/537.36")

PROVIDERS = {
    "nim-nvidia": "nim", "deepseek": "deepseek", "groq": "groq",
    "cerebras": "cerebras", "mistral": "mistral", "cohere": "cohere",
    "openrouter": "openrouter", "ai-router": "ai-router",
    "cloudflare-ai": "cloudflare-ai", "friendli": "friendli", "zen": "zen",
}

_wid, _wsec = open(WORKER_ID_FILE).read().strip().split("\n")[:2]
_key_cache = {}
_mod_cache = {}
_env_cache = {}


def _load(mod_name, path):
    if mod_name not in _mod_cache:
        _mod_cache[mod_name] = SourceFileLoader(
            mod_name, os.path.expanduser(path)).load_module()
    return _mod_cache[mod_name]


def keyman(name):
    if name not in _key_cache:
        out = subprocess.run([os.path.join(HOME, "workspace/bin/keyman.py"),
                              "get", name],
                             capture_output=True, text=True, check=True,
                             timeout=30)
        _key_cache[name] = out.stdout.strip()
    return _key_cache[name]


def env_file(path):
    if path not in _env_cache:
        d = {}
        with open(os.path.expanduser(path)) as f:
            for line in f:
                line = line.strip()
                if not line or line.startswith("#") or "=" not in line:
                    continue
                k, v = line.split("=", 1)
                d[k.strip()] = v.strip().strip('"').strip("'")
        _env_cache[path] = d
    return _env_cache[path]


def xchg(path, body, attempts=3):
    """POST to the Exchange with worker auth. Transport errors retry;
    HTTP responses (any status) are returned as-is - they are answers."""
    data = json.dumps(body).encode()
    last_err = None
    for i in range(attempts):
        req = urllib.request.Request(
            EXCHANGE + path, data=data, method="POST",
            headers={"Content-Type": "application/json",
                     "X-Worker-Id": _wid, "X-Worker-Secret": _wsec,
                     "User-Agent": "lucidota-exchange-worker/1.0 (phase3)"},
        )
        try:
            with urllib.request.urlopen(req, timeout=60) as r:
                return json.loads(r.read()), r.status
        except urllib.error.HTTPError as e:
            try:
                return json.loads(e.read().decode("utf-8", errors="replace")), e.code
            except Exception:
                return {"error": f"HTTP {e.code}"}, e.code
        except Exception as e:  # transport: RemoteDisconnected, timeout, DNS
            last_err = e
            if i + 1 < attempts:
                time.sleep(2 * (i + 1))
    return {"error": f"transport failed after {attempts} attempts: {type(last_err).__name__}: {str(last_err)[:120]}"}, 0


def load_envelope():
    return _load("envelope", "~/workspace/compute/envelope.py")


class _WallClockTimeout(TimeoutError):
    """Raised by the SIGALRM guard when an execute exceeds 300s."""


def _alarm_handler(signum, frame):
    raise _WallClockTimeout("provider call exceeded 300s wall-clock (SIGALRM)")


# Mechanical outcome table. Fixed, no judgment: HTTP status -> terminal.
# The worker reports what it mechanically observed; the governor classifies,
# verifies, and decides what happens next.
def to_outcome(http_status, has_content):
    if http_status == 200 and has_content:
        return "SUCCEEDED", None
    if http_status == 429:
        return "FAILED_RETRYABLE", "RATE_LIMITED"
    if http_status in (401, 403):
        return "FAILED_PERMANENT", "AUTH_ERROR"
    if http_status in (404, 410):
        return "FAILED_PERMANENT", "NOT_FOUND"
    if http_status == 400:
        return "FAILED_PERMANENT", "BAD_REQUEST"
    if http_status and 500 <= http_status < 600:
        return "FAILED_RETRYABLE", "SERVER_ERROR"
    if http_status == 200:
        return "FAILED_RETRYABLE", "BAD_RESPONSE"  # 200 with no usable content
    return "FAILED_RETRYABLE", "NETWORK"  # timeouts, DNS, connection resets


# ---------------------------------------------------------------------------
# Provider POST primitives. Each returns (http_status, raw_dict).
# status None = transport failure (no HTTP answer).
#
# TRANSPORT RETRY (v0.7.1, proven 2026-11-21): this egress path truncates
# reads (IncompleteRead) even on ~14KB responses. A truncated read is NOT
# a provider failure — the call may have executed server-side and we just
# lost the bytes. Provider POSTs retry 3x on transport errors only (never
# on HTTP answers); inference calls are side-effect-free, so a re-execute
# is $0 and honest. Zen subprocess calls do NOT retry (a hang is a
# model-down signal, not flakiness — retrying would burn another 300s).
# ---------------------------------------------------------------------------
_TRANSPORT_ERRORS = (http.client.IncompleteRead, http.client.RemoteDisconnected,
                     TimeoutError, ConnectionError, OSError)


def _retry_transport(fn, attempts=3):
    last = None
    for i in range(attempts):
        try:
            return fn()
        except _TRANSPORT_ERRORS as e:
            last = e
            if i + 1 < attempts:
                time.sleep(2 * (i + 1))
        except Exception as e:
            # Non-transport failure (surrogate, JSON, ...): no retry, but
            # never a crash — report it as a transport-shaped failure.
            return None, {"error": f"{type(e).__name__}: {str(e)[:200]}"}
    return None, {"error": f"{type(last).__name__}: {str(last)[:200]}"}


def _curl_post(url, headers, body, timeout=120):
    """POST via curl subprocess. Fallback when urllib truncates a read:
    curl's read loop survives the truncation urllib dies on (proven 3/3
    on the 14KB Cloudflare embedding response, 2026-11-21, where urllib
    truncated 3/3 with IncompleteRead). Headers go through a mode-600
    config file, never argv (they carry secrets). Returns (http_status,
    raw_dict) like the urllib primitives; raises on curl failure so the
    caller can fall back to retrying the urllib attempt."""
    import tempfile
    cfg = tempfile.NamedTemporaryFile("w", delete=False, prefix="xchg-curl-")
    try:
        os.chmod(cfg.name, 0o600)
        esc = lambda s: s.replace("\\", "\\\\").replace('"', '\\"')
        cfg.write(f'url = "{esc(url)}"\n')
        cfg.write('request = "POST"\n')
        for k, v in headers.items():
            cfg.write(f'header = "{esc(k)}: {esc(v)}"\n')
        cfg.write(f'max-time = {int(timeout)}\n')
        cfg.write('silent = true\n')
        cfg.write('show-error = true\n')
        cfg.write('write-out = "\\n__CURL_CODE__:%{http_code}"\n')
        cfg.close()
        cp = subprocess.run(["curl", "-K", cfg.name, "--data-binary", "@-"],
                            input=json.dumps(body).encode(),
                            capture_output=True, timeout=timeout + 10)
        out = cp.stdout.decode("utf-8", errors="replace")
        if "__CURL_CODE__:" not in out:
            raise RuntimeError(
                f"curl no status marker (rc={cp.returncode}): "
                f"{cp.stderr.decode(errors='replace')[:120]}")
        raw_text, code_s = out.rsplit("__CURL_CODE__:", 1)
        code = int(code_s.strip())
        if raw_text.endswith("\n"):
            raw_text = raw_text[:-1]
        try:
            raw = json.loads(raw_text) if raw_text.strip() else {}
        except Exception:
            raw = {"error": raw_text[:300]}
        if not isinstance(raw, dict):
            raw = {"error": str(raw)[:300]}
        return code, raw
    finally:
        try:
            os.unlink(cfg.name)
        except OSError:
            pass


def _post_json(url, headers, body, timeout=120):
    data = json.dumps(body).encode()

    def once():
        req = urllib.request.Request(url, data=data, method="POST", headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                return r.status, json.loads(r.read())
        except urllib.error.HTTPError as e:
            try:
                raw = json.loads(e.read().decode("utf-8", errors="replace"))
            except Exception:
                raw = {"error": f"HTTP {e.code}"}
            if not isinstance(raw, dict):
                raw = {"error": str(raw)[:300]}
            return e.code, raw
        except http.client.IncompleteRead as e:
            # urllib truncated the read; curl survives it (proven 3/3).
            # If curl fails too, re-raise the truncation so the retry
            # wrapper retries the urllib attempt.
            try:
                return _curl_post(url, headers, body, timeout)
            except Exception:
                raise e
    return _retry_transport(once)


def _module_post(call_fn, path, body):
    """Wrap a skill-module call (raises HTTPError on non-200)."""

    def once():
        try:
            return 200, call_fn(path, body)
        except urllib.error.HTTPError as e:
            try:
                raw = json.loads(e.read().decode("utf-8", errors="replace"))
            except Exception:
                raw = {"error": f"HTTP {e.code}"}
            if not isinstance(raw, dict):
                raw = {"error": str(raw)[:300]}
            return e.code, raw
    return _retry_transport(once)


def _surrogate_post(url, cred, host, body, timeout=120):
    """POST with an estate surrogate credential (deepseek shape)."""
    data = json.dumps(body).encode()

    def once():
        req = urllib.request.Request(url, data=data, method="POST",
                                     headers={"Content-Type": "application/json",
                                              "User-Agent": BROWSER_UA})
        try:
            add_surrogate_to_request(req, cred, entry_name="access_token",
                                     allowed_hosts=[host])
        except DynamicCredentialError:
            add_surrogate_to_request(req, cred, entry_name="acccess_token",
                                     allowed_hosts=[host])
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                return r.status, json.loads(r.read())
        except urllib.error.HTTPError as e:
            try:
                raw = json.loads(e.read().decode("utf-8", errors="replace"))
            except Exception:
                raw = {"error": f"HTTP {e.code}"}
            if not isinstance(raw, dict):
                raw = {"error": str(raw)[:300]}
            return e.code, raw
        except http.client.IncompleteRead as e:
            # urllib truncated the read; curl survives it (proven 3/3).
            # Headers (incl. the surrogate Authorization) come off the
            # request object — the surrogate is already resolved there.
            try:
                hdrs = dict(req.header_items())
                hdrs.update(req.unredirected_hdrs)
                return _curl_post(url, hdrs, body, timeout)
            except Exception:
                raise e
    return _retry_transport(once)


def provider_post(provider, path, body):
    """Route one POST to the provider's endpoint. path is the provider's
    own path (e.g. /chat/completions for OpenAI-compatible, /v2/chat for
    Cohere, /v1/chat for the CF proxy)."""
    if provider == "nim":
        nim = _load("nimcli", "~/workspace/skills/nvidia-nim/bin/nvidia-nim")
        return _module_post(lambda p, b: nim.api("POST", p, b), path, body)
    if provider == "groq":
        g = _load("groqcli", "~/workspace/skills/groq/bin/groq.py")
        return _module_post(lambda p, b: g._request("POST", p, b), path, body)
    if provider == "cerebras":
        c = _load("cerecli", "~/workspace/skills/cerebras/bin/cerebras.py")
        return _module_post(lambda p, b: c._request("POST", p, b), path, body)
    if provider == "deepseek":
        return _surrogate_post("https://api.deepseek.com/v1" + path,
                               "custom.deepseek", "api.deepseek.com", body)
    if provider == "mistral":
        return _post_json("https://api.mistral.ai/v1" + path,
                          {"Authorization": "Bearer " + keyman("MISTRAL_KEY"),
                           "Content-Type": "application/json",
                           "User-Agent": BROWSER_UA}, body)
    if provider == "cohere":
        return _post_json("https://api.cohere.com" + path,
                          {"Authorization": "Bearer " + keyman("COHERE_KEY"),
                           "Content-Type": "application/json",
                           "User-Agent": BROWSER_UA}, body)
    if provider == "openrouter":
        return _post_json("https://openrouter.ai/api/v1" + path,
                          {"Authorization": "Bearer " + keyman("OPENROUTER_KEY"),
                           "Content-Type": "application/json",
                           "X-Title": "lucidota",
                           "User-Agent": BROWSER_UA}, body)
    if provider == "ai-router":
        rkey = open(os.path.join(HOME, "workspace/keys/ROUTER_KEY")).read().strip()
        return _post_json("https://ai-router.cutecity.workers.dev/v1" + path,
                          {"Authorization": "Bearer " + rkey,
                           "Content-Type": "application/json",
                           "User-Agent": BROWSER_UA}, body)
    if provider == "cloudflare-ai":
        return _post_json("https://lucidota-ai.cutecity.workers.dev" + path,
                          {"x-proxy-key": keyman("CF_AI_PROXY_KEY"),
                           "Content-Type": "application/json",
                           "User-Agent": BROWSER_UA}, body)
    if provider == "friendli":
        fenv = env_file("~/workspace/wreck/.env")
        return _post_json("https://api.friendli.ai/serverless/v1" + path,
                          {"Authorization": "Bearer " + fenv["FRIENDLI_API_KEY"],
                           "Content-Type": "application/json",
                           "User-Agent": BROWSER_UA}, body)
    raise ValueError(f"no POST primitive for provider {provider}")


def _openai_content(raw):
    """Extract (content, model_actual, usage) from an OpenAI-shaped reply.
    Reasoning models answer in reasoning_content with content=null."""
    ch = (raw.get("choices") or [{}])[0]
    msg = ch.get("message") or {}
    content = msg.get("content") or msg.get("reasoning_content") or msg.get("reasoning")
    return content, raw.get("model"), raw.get("usage")


def _zen_chat(model, messages, max_tokens, temperature):
    """Zen via opencode run. stdin DEVNULL + timeout are MANDATORY:
    opencode hangs silently forever without stdin redirect, and hangs
    silently on unavailable models (AGENTS.md, ledger 15964).
    model_actual is invocation-bound: opencode executes the --model it
    was given or fails; it never substitutes silently."""
    prompt = "\n".join(f"{m.get('role', 'user')}: {m.get('content', '')}"
                       for m in messages)
    fenv = env_file("~/workspace/wreck/.env")
    env = dict(os.environ)
    env["OPENCODE_API_KEY"] = fenv["OPENCODE_API_KEY"]
    # Model ids are stored with the opencode/ prefix (as listed); pass
    # verbatim — never double-prefix.
    model_arg = model if model.startswith("opencode/") else f"opencode/{model}"
    t0 = time.time()
    try:
        p = subprocess.run(
            [os.path.join(HOME, ".opencode/bin/opencode"), "run",
             "--model", model_arg, "--auto", prompt],
            capture_output=True, text=True, timeout=300,
            stdin=subprocess.DEVNULL, env=env,
            cwd=os.path.join(HOME, "workspace/wreck"))
    except subprocess.TimeoutExpired:
        return None, {"error": "opencode run timed out after 300s"}, 0, model, None
    lat = int((time.time() - t0) * 1000)
    out = (p.stdout or "").strip()
    err = (p.stderr or "").strip()
    if p.returncode == 0 and out:
        return 200, {"content": out, "model": model}, lat, model, None
    if "unavailable" in (out + err).lower():
        return 404, {"error": (out + err)[:300] or "model unavailable"}, lat, None, None
    return 500, {"error": (err or out or f"rc={p.returncode}")[:300]}, lat, None, None


def run_inference(provider, model, messages, max_tokens, temperature=0.7):
    """Exactly one provider call for the requested model. NO fallback:
    404/410 is a terminal NOT_FOUND, reported honestly."""
    t0 = time.time()
    if provider == "zen":
        status, raw, lat, model_actual, usage = _zen_chat(
            model, messages, max_tokens, temperature)
        content = raw.get("content") if status == 200 else None
        outcome, error_class = to_outcome(status, bool(content))
        return {"worker_outcome": outcome, "error_class": error_class,
                "error": None if outcome == "SUCCEEDED" else str(raw.get("error", f"http {status}"))[:300],
                "output": {"content": content} if outcome == "SUCCEEDED" else None,
                "latency_ms": lat, "model_actual": model_actual,
                "model_requested": model, "model_used": model}

    if provider == "cohere":
        # Cohere v2 chat: OpenAI-shaped request, block-shaped response.
        # No model echo in the reply: the call executed the requested
        # model or errored; model_actual is invocation-bound.
        status, raw = provider_post(provider, "/v2/chat", {
            "model": model, "messages": messages,
            "max_tokens": max_tokens, "temperature": temperature})
        lat = int((time.time() - t0) * 1000)
        content, usage = None, raw.get("usage") if isinstance(raw, dict) else None
        if status == 200 and isinstance(raw, dict):
            blocks = ((raw.get("message") or {}).get("content") or [])
            texts = [b.get("text", "") for b in blocks if b.get("type") == "text"]
            content = "".join(texts) or None
        outcome, error_class = to_outcome(status, bool(content))
        return {"worker_outcome": outcome, "error_class": error_class,
                "error": None if outcome == "SUCCEEDED" else str((raw or {}).get("message", raw.get("error", f"http {status}")) if isinstance(raw, dict) else f"http {status}")[:300],
                "output": {"content": content, "usage": usage} if outcome == "SUCCEEDED" else None,
                "latency_ms": lat, "model_actual": model if outcome == "SUCCEEDED" else None,
                "model_requested": model, "model_used": model}

    if provider == "cloudflare-ai":
        # Proxy: POST /v1/chat {messages, model, max_tokens} ->
        # {model, response}. The proxy passes any model id through.
        status, raw = provider_post(provider, "/v1/chat", {
            "model": model, "messages": messages, "max_tokens": max_tokens})
        lat = int((time.time() - t0) * 1000)
        content, model_actual, usage = None, None, None
        if status == 200 and isinstance(raw, dict):
            resp = raw.get("response")
            content = resp if isinstance(resp, str) else (json.dumps(resp) if resp is not None else None)
            model_actual = raw.get("model")
            usage = raw.get("usage")
        outcome, error_class = to_outcome(status, bool(content))
        return {"worker_outcome": outcome, "error_class": error_class,
                "error": None if outcome == "SUCCEEDED" else str((raw or {}).get("message", (raw or {}).get("error", f"http {status}")))[:300],
                "output": {"content": content, "usage": usage} if outcome == "SUCCEEDED" else None,
                "latency_ms": lat, "model_actual": model_actual,
                "model_requested": model, "model_used": model}

    # OpenAI-compatible chat completions (nim, deepseek, groq, cerebras,
    # mistral, openrouter, ai-router, friendli).
    status, raw = provider_post(provider, "/chat/completions", {
        "model": model, "messages": messages,
        "max_tokens": max_tokens, "temperature": temperature, "stream": False})
    lat = int((time.time() - t0) * 1000)
    content, model_actual, usage = None, None, None
    if status == 200 and isinstance(raw, dict):
        content, model_actual, usage = _openai_content(raw)
    outcome, error_class = to_outcome(status, bool(content))
    return {"worker_outcome": outcome, "error_class": error_class,
            "error": None if outcome == "SUCCEEDED" else str((raw or {}).get("error", f"http {status}"))[:300],
            "output": {"content": content,
                       "finish_reason": ((raw.get("choices") or [{}])[0].get("finish_reason")) if isinstance(raw, dict) else None,
                       "usage": usage} if outcome == "SUCCEEDED" else None,
            "latency_ms": lat, "model_actual": model_actual,
            "model_requested": model, "model_used": model}


def run_embedding(provider, model, texts):
    """Exactly one embedding call. Output is the raw vectors — the
    governor persists them; this worker never interprets them."""
    t0 = time.time()
    if provider == "cohere":
        status, raw = provider_post(provider, "/v2/embed", {
            "model": model, "texts": texts, "input_type": "search_document"})
        lat = int((time.time() - t0) * 1000)
        vecs = ((raw.get("embeddings") or {}).get("float") or []) if status == 200 and isinstance(raw, dict) else []
        outcome, error_class = to_outcome(status, bool(vecs))
        return {"worker_outcome": outcome, "error_class": error_class,
                "error": None if outcome == "SUCCEEDED" else str((raw or {}).get("message", f"http {status}"))[:300],
                "output": {"embeddings": vecs, "count": len(vecs)} if outcome == "SUCCEEDED" else None,
                "latency_ms": lat, "model_actual": model if outcome == "SUCCEEDED" else None,
                "model_requested": model, "model_used": model}
    if provider == "cloudflare-ai":
        status, raw = provider_post(provider, "/v1/embeddings", {
            "model": model, "input": texts})
        lat = int((time.time() - t0) * 1000)
        vecs = [d.get("embedding") for d in (raw.get("data") or [])] if status == 200 and isinstance(raw, dict) else []
        outcome, error_class = to_outcome(status, bool(vecs))
        return {"worker_outcome": outcome, "error_class": error_class,
                "error": None if outcome == "SUCCEEDED" else str((raw or {}).get("message", (raw or {}).get("error", f"http {status}")))[:300],
                "output": {"embeddings": vecs, "count": len(vecs), "dims": raw.get("dims")} if outcome == "SUCCEEDED" else None,
                "latency_ms": lat, "model_actual": raw.get("model") if isinstance(raw, dict) else None,
                "model_requested": model, "model_used": model}
    if provider in ("nim", "mistral", "openrouter"):
        if provider == "nim":
            # Direct surrogate POST, NOT the skill module: the module's
            # urllib read truncates the ~35KB 2048-dim response on this
            # egress path, and the module call is opaque to the curl
            # fallback. The direct call gets _surrogate_post's curl
            # fallback. (Skips the module's local call log; the Exchange
            # receipt is the record that matters.)
            status, raw = _surrogate_post(
                "https://integrate.api.nvidia.com/v1/embeddings",
                "custom.nvidia-nim", "integrate.api.nvidia.com",
                {"model": model, "input": texts})
        else:
            status, raw = provider_post(provider, "/embeddings", {
                "model": model, "input": texts})
        lat = int((time.time() - t0) * 1000)
        vecs = [d.get("embedding") for d in (raw.get("data") or [])] if status == 200 and isinstance(raw, dict) else []
        outcome, error_class = to_outcome(status, bool(vecs))
        return {"worker_outcome": outcome, "error_class": error_class,
                "error": None if outcome == "SUCCEEDED" else str((raw or {}).get("error", f"http {status}"))[:300],
                "output": {"embeddings": vecs, "count": len(vecs)} if outcome == "SUCCEEDED" else None,
                "latency_ms": lat, "model_actual": raw.get("model") if isinstance(raw, dict) else None,
                "model_requested": model, "model_used": model}
    # Provider has no embedding path in this claimant (the governor's
    # MODEL gate should never route an embedding job here).
    return {"worker_outcome": "FAILED_PERMANENT", "error_class": "BAD_REQUEST",
            "error": f"provider {provider} has no embedding path in this claimant",
            "output": None, "latency_ms": 0, "model_actual": None,
            "model_requested": model, "model_used": model}


def mint_envelope(envelope_mod, claim_id, res, job_id):
    """Seal the completion envelope. claim_id binds the envelope to THIS
    request (checkClaimBinding, complete route). Payload is evidence, not
    authority: the governor re-derives everything from the request body."""
    output_hash = None
    if res.get("output") is not None:
        output_hash = hashlib.sha256(
            json.dumps(res["output"], sort_keys=True,
                       separators=(",", ":"), ensure_ascii=False).encode("utf-8")
        ).hexdigest()
    payload = {
        "worker_outcome": res["worker_outcome"],
        "resource_id": RESOURCE,
        "job_id": job_id,
        "model_actual": res.get("model_actual") or "",
        "latency_ms": int(res["latency_ms"]),
        "output_hash": output_hash or "",
    }
    return envelope_mod.wrap(actor=_wid, claim_id=claim_id,
                             status="established", payload=payload)


def report(envelope_mod, claim, d, res):
    """CAPTURE + REPORT — one terminal outcome + sealed envelope."""
    env = mint_envelope(envelope_mod, d["claim_id"], res, claim["job_id"])
    body = {"claim_id": d["claim_id"],
            "worker_outcome": res["worker_outcome"],
            "error_class": res["error_class"],
            "error": res["error"],
            "output": res["output"],
            "latency_ms": res["latency_ms"],
            "model_actual": res["model_actual"],
            "envelope": env}
    done, code = xchg("/v1/complete", body)
    if code == 409 and "replay" in str(done.get("error") or ""):
        # The first complete attempt landed server-side; its response
        # was lost to transport and the retry hit replay protection.
        # The completion IS recorded — the 409 proves it (the envelope
        # is claim_id-bound to this worker; a replay of MY envelope
        # means MY first attempt landed). The worker cannot fetch the
        # job doc (no Exchange key by design); the governor's job doc
        # is the verification. Report honestly, not as a failure.
        print(json.dumps({"completed": claim["job_id"], "claim_id": d["claim_id"],
                          "outcome": res["worker_outcome"],
                          "model_requested": res["model_requested"],
                          "model_used": res["model_used"],
                          "model_actual": res["model_actual"],
                          "state": "done (replay-confirmed)",
                          "receipt_id": None,
                          "taxonomy": "replay — first attempt recorded",
                          "idempotent": True, "http": 409,
                          "error": done.get("error")}), flush=True)
        return
    print(json.dumps({"completed": claim["job_id"], "claim_id": d["claim_id"],
                      "outcome": res["worker_outcome"],
                      "model_requested": res["model_requested"],
                      "model_used": res["model_used"],
                      "model_actual": res["model_actual"],
                      "state": done.get("state"), "receipt_id": done.get("receipt_id"),
                      "taxonomy": done.get("taxonomy"),
                      "idempotent": done.get("idempotent", False),
                      "http": code,
                      "error": done.get("error")}), flush=True)


def refuse(envelope_mod, claim, d, error_class, error, model_requested):
    """Refuse a delegation WITHOUT executing. The governor records the
    refusal; nothing was spent, nothing was substituted."""
    res = {"worker_outcome": "FAILED_PERMANENT", "error_class": error_class,
           "error": error, "output": None, "latency_ms": 0,
           "model_actual": None, "model_requested": model_requested,
           "model_used": model_requested}
    print(json.dumps({"refused": claim["job_id"], "claim_id": d["claim_id"],
                      "error_class": error_class, "error": error}), flush=True)
    report(envelope_mod, claim, d, res)


def run_one(envelope_mod):
    """PICKUP -> VALIDATE -> EXECUTE -> CAPTURE -> REPORT, one job.
    Returns (did_work: bool, fatal: bool)."""
    provider = PROVIDERS.get(RESOURCE)
    if provider is None:
        print(json.dumps({"error": f"unknown resource {RESOURCE} — no provider mapping; refusing to claim"}), flush=True)
        return False, True
    claim, code = xchg("/v1/claim", {"resource_id": RESOURCE}, attempts=1)
    if code == 0:
        # Transport failed: the claim may have landed server-side while
        # the response was lost (truncated read — proven 2026-11-21, 3
        # orphans). Resume it (worker v0.7.2): the server returns MY live
        # claim delegation, or falls through to a fresh claim if none
        # landed. Never blind-retry a claim — the retry used to return
        # claimed:false and orphan the lease.
        print(json.dumps({"claim_transport_retry": claim.get("error")}), flush=True)
        claim, code = xchg("/v1/claim", {"resource_id": RESOURCE, "resume": True}, attempts=2)
    if code != 200:
        print(json.dumps({"claim_error": claim.get("error"), "http": code}), flush=True)
        return False, True
    if not claim.get("claimed"):
        return False, False
    d = claim["delegation"]
    # VALIDATE - structural only. The worker cannot fix a bad delegation,
    # only refuse it honestly.
    for f in ("claim_id", "input", "lease_expiry", "policy_snapshot"):
        if f not in d or d[f] is None:
            print(json.dumps({"error": f"delegation missing {f} - refusing"}))
            return True, True
    if d["lease_expiry"] < time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()):
        print(json.dumps({"error": "delegation already expired - refusing"}))
        return True, True
    inp = d["input"] or {}
    # EXACT-MODEL BINDING — the delegation must agree with the job.
    # Any disagreement is refused WITHOUT executing (see module docstring).
    job_model = inp.get("model")
    del_model = d.get("requested_model")
    model_exact = d.get("model_exact", True)
    if job_model and del_model and job_model != del_model:
        refuse(envelope_mod, claim, d, "MODEL_MISMATCH",
               f"delegation model {del_model} != job model {job_model} — refusing without executing",
               job_model)
        return True, False
    model = job_model or del_model
    if not model:
        refuse(envelope_mod, claim, d, "MODEL_MISMATCH",
               "no model bound (job pins none, delegation names none) — refusing without executing",
               None)
        return True, False
    if job_model and not model_exact:
        refuse(envelope_mod, claim, d, "MODEL_MISMATCH",
               "pinned model on a failover-pool resource (model_exact=false) — refusing without executing",
               job_model)
        return True, False
    # EXECUTE - exactly one provider call. No retries, no provider
    # failover, no model fallback. The governor replans.
    # WALL-CLOCK GUARD (2026-11-21): three hook workers wedged ~20 min
    # with 0 CPU (cause unproven — no logs survived; DNS stall is the
    # best hypothesis). Provider calls have socket timeouts, but a stall
    # outside them (DNS, a module without a timeout) hangs forever. A
    # 300s SIGALRM bounds any execute; the timeout is reported as a
    # retryable NETWORK failure, never a crash. Zen subprocess calls
    # already bound themselves at 300s (PEP 475 retries the wait past
    # the alarm — the subprocess timeout is the operative bound there).
    capability = d.get("capability") or "inference"
    signal.alarm(300)
    try:
        if capability == "embedding":
            texts = inp.get("input") or inp.get("texts")
            if isinstance(texts, str):
                texts = [texts]
            if not texts:
                refuse(envelope_mod, claim, d, "BAD_REQUEST",
                       "embedding job has no input texts — refusing without executing",
                       model)
                return True, False
            res = run_embedding(provider, model, texts)
        elif capability == "inference":
            messages = inp.get("messages") or [{"role": "user", "content": str(inp)}]
            res = run_inference(provider, model, messages, inp.get("max_tokens", 512),
                                inp.get("temperature", 0.7))
        else:
            refuse(envelope_mod, claim, d, "BAD_REQUEST",
                   f"claimant serves inference/embedding only, not {capability} — refusing without executing",
                   model)
            return True, False
    except _WallClockTimeout as e:
        res = {"worker_outcome": "FAILED_RETRYABLE", "error_class": "NETWORK",
               "error": str(e)[:200], "output": None, "latency_ms": 300000,
               "model_actual": None, "model_requested": model, "model_used": model}
    finally:
        signal.alarm(0)
    report(envelope_mod, claim, d, res)
    return True, False


def main():
    signal.signal(signal.SIGALRM, _alarm_handler)
    envelope_mod = load_envelope()
    # DRAIN - claim until no work or cap. The governor's claim is atomic,
    # so parallel workers never double-execute; each drains independently.
    drained, fatal = 0, False
    while drained < DRAIN_CAP:
        did_work, bad = run_one(envelope_mod)
        if bad:
            fatal = True
            break
        if not did_work:
            break
        drained += 1
    print(json.dumps({"drained": drained, "cap": DRAIN_CAP,
                      "idle": drained == 0, "resource": RESOURCE,
                      "provider": PROVIDERS.get(RESOURCE),
                      "worker": _wid}))
    return 1 if fatal else 0


if __name__ == "__main__":
    sys.exit(main())
