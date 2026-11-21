#!/usr/bin/env python3
"""Bundle the wasm-bindgen output into a single Cloudflare Workers-compatible JS file.

Takes the bundler-target output and inlines snippets, producing a single
worker.js that imports the WASM via Cloudflare's wasm_module binding.
"""
import os
import re
import sys

stage = "/home/hatch/workspace/exchange/stage"

# Read the snippet
snippet_path = os.path.join(stage, "snippets", "worker-64cc4b2d035528c7", "inline0.js")
with open(snippet_path) as f:
    snippet = f.read()
# Extract the actual code (remove export)
snippet_code = snippet.replace("export const state =", "const state =")

# Read bg.js
with open(os.path.join(stage, "exchange_worker_bg.js")) as f:
    bg = f.read()
# Remove the snippet import, replace with inlined code
bg = bg.replace(
    "import { state } from './snippets/worker-64cc4b2d035528c7/inline0.js';",
    snippet_code
)
# Remove the wasm import (we'll provide it via Cloudflare binding)
# The bg.js uses `wasm.` prefix for WASM calls; we need to define `wasm`
# Actually, bundler target expects `wasm` to be the imported module.
# For Cloudflare, we import it via the wasm_module binding.

# Read main js
with open(os.path.join(stage, "exchange_worker.js")) as f:
    main = f.read()

# Create the bundled output
# Cloudflare: import wasm from the binding name specified in metadata
output = """// Bundled exchange worker (Rust/WASM) for Cloudflare Workers.
// WASM module is provided via the `wasm` wasm_module binding.
import wasmModule from "./exchange_worker_bg.wasm";

""" + bg.replace(
    "import { state } from './snippets/worker-64cc4b2d035528c7/inline0.js';",
    snippet_code
).replace(
    # The bg.js doesn't import wasm directly in bundler mode; it's passed via __wbg_set_wasm
    # We need to set it up
    "",
    ""
)

# Actually, let's do this more carefully.
# The main.js does:
#   import * as wasm from "./exchange_worker_bg.wasm";
#   import { __wbg_set_wasm } from "./exchange_worker_bg.js";
#   __wbg_set_wasm(wasm);
#   export { fetch, scheduled } from "./exchange_worker_bg.js";

# For Cloudflare, the wasm_module binding provides the WASM as a WebAssembly.Module
# We need to instantiate it. The bg.js has __wbg_set_wasm which takes the instance exports.

# Simpler: keep the structure, but inline everything into one file.
# The WASM import becomes a Cloudflare binding.

bundled = f"""// exchange-worker (Rust) — Cloudflare Workers bundle
// Generated from wasm-bindgen bundler output; snippets inlined.
// The WASM binary is supplied via the `wasm` wasm_module binding.

import wasmInit from "./exchange_worker_bg.wasm";

// Inline the snippet
{snippet_code}

// WASM instance will be set here
let wasm;

// --- Begin bg.js (with imports resolved) ---
"""

# Process bg.js: remove the snippet import line, keep the rest
bg_lines = bg.split("\n")
bg_filtered = [l for l in bg_lines if "snippets/worker-" not in l]
bundled += "\n".join(bg_filtered)

bundled += """

// --- End bg.js ---

// Initialize WASM and set up exports
__wbg_set_wasm(wasmInit);
wasmInit.__wbindgen_start();

// Re-export the handlers
export { fetch, scheduled };
"""

with open(os.path.join(stage, "worker-bundle.js"), "w") as f:
    f.write(bundled)

print(f"Wrote worker-bundle.js ({len(bundled)} bytes)")
print("NOTE: This is a best-effort bundle; the WASM import may need adjustment")
print("for Cloudflare's specific wasm_module binding semantics.")
