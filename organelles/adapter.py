#!/usr/bin/env python3
"""exchange organelle surface adapter (Tier B).

Thin unix-socket JSON-lines adapter between the organelle-host and this repo's
real entrypoints. Computation stays in its own language; authority, receipts,
kill, and resource envelope are enforced by the host.

Default socket path: $XDG_RUNTIME_DIR/organelle-host.sock
(env ORGANELLE_SOCK or --sock overrides).

Wire protocol (newline-delimited JSON):
  request:  {{"id":N,"caller":"...","capabilities":[],"codon":"<2-char>","op":"<op>","input":{{...}}}}
  response: {{"id":N,"ok":bool,"output":{{...}}|null}}

Op -> entrypoint mapping is mechanical: input keys become --kebab-case flags
(bool True -> bare flag; list -> repeated flag; other values -> --key=value,
dicts JSON-encoded). Dir-mode units have no single entrypoint and return a
structured error naming the repo harness. No business logic lives here.

Special ops (handled internally, never subprocessed):
  suspend -> ok with checkpoint note (state is ephemeral)
  kill    -> final receipt then the adapter exits

Usage:
  adapter.py [--sock PATH]            # serve forever on the socket
  echo '<json request>' | adapter.py --once [--sock PATH]
"""

import argparse, json, os, re, signal, socket, subprocess, sys

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SOCK_DEFAULT = os.path.join(os.environ.get("XDG_RUNTIME_DIR", "/tmp"), "organelle-host.sock")
TIMEOUT_SECS = 600  # matches execution.json resource_envelope.wall_time_secs

# (codon, op) -> (mode, entry). mode: py | sh | bin | delegate | py-template | dir
DISPATCH = {
    ('ex', 'claim'): ('py', 'mint-claimant.py'),
    ('ex', 'compute'): ('py', 'intelligence-per-byte.py'),
    ('ex', 'discover'): ('py', 'discover-local.py'),
    ('ex', 'execute'): ('py', 'mint-worker.py'),
    ('ex', 'govern'): ('py', 'deploy-governor.py'),
    ('ex', 'probe'): ('py', 'probe-harness.py'),
    ('ex', 'refresh'): ('py', 'refresh-mirror.py'),
    ('ex', 'rollup'): ('py', 'ballot-cost-rollup.py'),
    ('ex', 'submit'): ('py', 'mint-submit.py'),
    ('ex', 'sync'): ('py', 'evidence-graph-sync.py'),
}

def kebab(k):
    return "--" + re.sub(r"_", "-", k)

def build_argv(mode, entry, op_input):
    if mode == "py-template":
        entry = entry.format(**{k: str(v) for k, v in op_input.items()})
    target = os.path.normpath(os.path.join(REPO_ROOT, entry))
    if mode in ("py", "py-template", "delegate"):
        argv = [sys.executable, target]
    elif mode == "sh":
        argv = ["bash", target]
    elif mode == "bin":
        argv = [target]
    else:
        raise ValueError("unrunnable mode: %s" % mode)
    for k, v in op_input.items():
        if mode == "py-template" and ("{" + k + "}") in entry:
            continue
        if isinstance(v, bool):
            if v: argv.append(kebab(k))
        elif isinstance(v, list):
            for item in v:
                argv += [kebab(k), str(item)]
        elif isinstance(v, dict):
            argv += [kebab(k), json.dumps(v)]
        elif v is None:
            continue
        else:
            argv += [kebab(k), str(v)]
    return argv

def handle(req):
    rid = req.get("id")
    codon, op, op_input = req.get("codon"), req.get("op"), req.get("input") or {}
    if not isinstance(rid, int) or not codon or not op or not isinstance(op_input, dict):
        return {"id": rid, "ok": False, "output": {"error": "malformed request"}}
    if op == "suspend":
        return {"id": rid, "ok": True,
                "output": {"checkpoint": "ephemeral",
                           "note": "in-flight op drained; adapter state is ephemeral"}}
    if op == "kill":
        return {"id": rid, "ok": True,
                "output": {"final": True, "note": "final receipt; adapter exiting"}}
    key = (codon, op)
    if key not in DISPATCH:
        return {"id": rid, "ok": False, "output": {"error": "unknown op %s/%s" % (codon, op)}}
    mode, entry = DISPATCH[key]
    if mode == "dir":
        return {"id": rid, "ok": False,
                "output": {"error": "dir-mode unit: no single entrypoint; invoke via the repo harness",
                           "unit_dir": entry}}
    try:
        argv = build_argv(mode, entry, op_input)
    except Exception as e:
        return {"id": rid, "ok": False, "output": {"error": "argv build failed: %s" % e}}
    try:
        p = subprocess.run(argv, capture_output=True, text=True,
                           timeout=TIMEOUT_SECS, cwd=REPO_ROOT)
        out = {"stdout": p.stdout[-8000:], "stderr": p.stderr[-4000:], "exit_code": p.returncode}
        return {"id": rid, "ok": p.returncode == 0, "output": out}
    except subprocess.TimeoutExpired:
        return {"id": rid, "ok": False,
                "output": {"error": "timeout after %ss (suspend-first: host retries)" % TIMEOUT_SECS}}
    except Exception as e:
        return {"id": rid, "ok": False, "output": {"error": "spawn failed: %s" % e}}

def serve(sock_path):
    if os.path.exists(sock_path):
        try: os.unlink(sock_path)
        except OSError: pass
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(sock_path); srv.listen(16)
    shutdown = {"flag": False}
    def _stop(signum, frame):
        shutdown["flag"] = True
    signal.signal(signal.SIGTERM, _stop); signal.signal(signal.SIGINT, _stop)
    srv.settimeout(1.0)
    while not shutdown["flag"]:
        try:
            conn, _ = srv.accept()
        except socket.timeout:
            continue
        with conn:
            buf = b""
            while True:
                chunk = conn.recv(65536)
                if not chunk: break
                buf += chunk
                while b"\n" in buf:
                    line, buf = buf.split(b"\n", 1)
                    if not line.strip(): continue
                    try:
                        resp = handle(json.loads(line))
                    except Exception as e:
                        resp = {"id": None, "ok": False, "output": {"error": "bad json: %s" % e}}
                    conn.sendall((json.dumps(resp) + "\n").encode())
                    if resp.get("output", {}).get("final"):
                        shutdown["flag"] = True
                        break
                if shutdown["flag"]: break
    srv.close()
    try: os.unlink(sock_path)
    except OSError: pass

def main():
    ap = argparse.ArgumentParser(description="Tier-B organelle surface adapter")
    ap.add_argument("--sock", default=os.environ.get("ORGANELLE_SOCK", SOCK_DEFAULT))
    ap.add_argument("--once", action="store_true", help="handle one JSON request from stdin")
    args = ap.parse_args()
    if args.once:
        line = sys.stdin.readline()
        try:
            resp = handle(json.loads(line))
        except Exception as e:
            resp = {"id": None, "ok": False, "output": {"error": "bad json: %s" % e}}
        print(json.dumps(resp), flush=True)
        return
    serve(args.sock)

if __name__ == "__main__":
    main()
