#!/usr/bin/env python3
"""Tier-B organelle validation tests for exchange.
Run: python3 test_organelles.py   (pytest-compatible: collects as module tests)
"""
import glob, json, os, re, subprocess, sys

ROOT = os.path.dirname(os.path.abspath(__file__))
SCHEMA = json.load(open(os.path.expanduser("~/workspace/organelles/manifest-json-schema.json")))
sys.path.insert(0, ROOT)
import adapter

def _load_manifests():
    out = []
    for p in sorted(glob.glob(os.path.join(ROOT, "*.manifest.json"))):
        out.append((p, json.load(open(p))))
    assert out, "no manifests found"
    return out

def _fail(msg):
    raise AssertionError(msg)

def test_manifests_ok():
    """Every manifest validates against the organelle-manifest v1 schema,
    and Write/ExternalEffect ops carry granted capabilities."""
    try:
        import jsonschema
        from jsonschema import Draft202012Validator
        v = Draft202012Validator(SCHEMA)
        have_js = True
    except ImportError:
        have_js, v = False, None
    for p, m in _load_manifests():
        unit = os.path.basename(p).replace(".manifest.json", "")
        assert m["name"].endswith("-" + unit) or m["name"] == "exchange-" + unit, "name/file mismatch: %s" % p
        if have_js:
            errs = list(v.iter_errors(m))
            assert not errs, "%s: %s" % (p, [e.message for e in errs][:3])
        else:
            for f in ("schema_version","name","codon","version","description","contracts","authority","transports","receipt_policy"):
                assert f in m, "%s missing %s" % (p, f)
        caps = set(m["authority"].get("granted_capabilities", []))
        for op in m["contracts"]:
            mc = op.get("mutation_class", m["authority"]["mutation_class"])
            if mc in ("write", "external_effect"):
                assert op.get("required_capability") in caps, "%s op %s %s lacks granted capability" % (p, op["op"], mc)

def test_dispatch_cov():
    """Adapter dispatch table covers every manifest (codon, op)."""
    for p, m in _load_manifests():
        for op in m["contracts"]:
            assert (m["codon"], op["op"]) in adapter.DISPATCH, "unmapped %s/%s" % (m["codon"], op["op"])

def test_exec_json_ok():
    """execution.json declares fs, network, secrets, envelope, kill semantics."""
    e = json.load(open(os.path.join(ROOT, "execution.json")))
    for k in ("filesystem","network","secrets","resource_envelope","kill_semantics","sandbox","repo_head"):
        assert k in e, "execution.json missing %s" % k
    env = e["resource_envelope"]
    for k in ("cpu_cores","mem_mb","wall_time_secs","io_mb"):
        assert k in env, "envelope missing %s" % k
    assert e["kill_semantics"].get("final_receipt") is True

def test_adapter_smoke():
    """Adapter --once handles the suspend special op over the wire format."""
    req = {"id": 7, "caller": "test", "capabilities": [], "codon": "ex", "op": "suspend", "input": {}}
    p = subprocess.run([sys.executable, os.path.join(ROOT, "adapter.py"), "--once"],
                       input=json.dumps(req) + "\n", capture_output=True, text=True, timeout=30)
    resp = json.loads(p.stdout.strip().splitlines()[-1])
    assert resp["id"] == 7 and resp["ok"] is True, resp
    assert resp["receipt"].startswith("adapter-interim:sha256:"), resp
    assert resp["error"] is None, resp

def test_no_secrets():
    """No secret-shaped material in manifests."""
    pat = re.compile(r"(sk-|ghp_|xox[bap]-|AKIA|BEGIN [A-Z ]*PRIVATE KEY|-----BEGIN)", re.I)
    for p, m in _load_manifests():
        blob = json.dumps(m)
        assert not pat.search(blob), "secret-shaped material in %s" % p
        for op in m["contracts"]:
            assert len(op["op"]) <= 64

TESTS = [test_manifests_ok, test_dispatch_cov, test_exec_json_ok, test_adapter_smoke, test_no_secrets]

def main():
    fails = 0
    for t in TESTS:
        try:
            t(); print("PASS %s" % t.__name__)
        except Exception as e:
            fails += 1; print("FAIL %s: %s" % (t.__name__, e))
    print("%d/%d passed" % (len(TESTS)-fails, len(TESTS)))
    sys.exit(1 if fails else 0)

if __name__ == "__main__":
    main()
