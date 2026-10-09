/**
 * lucidota-receipt/1 envelope verifier — JS port (workerd-compatible).
 *
 * Byte-faithful port of the VERIFY path of ~/workspace/proof-ledger/rs/src/envelope.rs
 * (the reference implementation; Python port: ~/workspace/compute/envelope.py).
 * This module is VERIFY-ONLY: minting stays in Rust/Python. New code MUST NOT
 * invent a second envelope — extend this one (see RECEIPT-ENVELOPE.md).
 *
 * Canonical JSON contract (pinned 2026-10-08, ENVELOPE-XIMPL):
 *   - UTF-8 raw (no \uXXXX escapes for non-ASCII), keys sorted by UTF-8 byte
 *     order, compact separators ("," ":"), no floats, integers in i64 range.
 * sha256 via WebCrypto (works in workerd and Node 18+); sig is "HS256:<hex>"
 * HMAC-SHA256 over the canonical envelope minus `seal` and minus `sig`,
 * keyed by the authority key. Verifiers dispatch on the "ALG:" prefix and
 * reject unknown algorithms. The authority key is PASSED IN (from a
 * Cloudflare secret at runtime) — never read from disk here, never logged.
 */

export const ENV_VERSION = "lucidota-receipt/1";
export const GENESIS = "GENESIS";
export const STATUSES = ["claimed", "established", "authorized"];
const STATUS_SET = new Set(STATUSES);
export const SIG_ALG = "HS256";
const I64_MIN = -9223372036854775808; // -2^63, exactly representable
const I64_MAX = 9223372036854775807;  // 2^63-1 (rounds to 2^63 in f64; the
                                      // comparison below stays fail-closed)

export class EnvelopeError extends Error {
  constructor(message) {
    super(message);
    this.name = "EnvelopeError";
  }
}

const _te = new TextEncoder();

/** Key sort: UTF-8 byte order (matches Rust BTreeMap<&str> / Python sort_keys
 *  on valid UTF-8; JS default sort() uses UTF-16 code units and diverges on
 *  non-BMP characters). */
function cmpUtf8(a, b) {
  const ab = _te.encode(a), bb = _te.encode(b);
  const n = Math.min(ab.length, bb.length);
  for (let i = 0; i < n; i++) {
    if (ab[i] !== bb[i]) return ab[i] - bb[i];
  }
  return ab.length - bb.length;
}

/**
 * Canonical JSON. Mirrors compute/envelope.py canonical() exactly:
 * sorted keys, compact separators, raw UTF-8, no floats, i64-range integers.
 * JSON.stringify's string escaping matches Python/Rust on the escapable set
 * (", \, U+0000-U+001F with \b \t \n \f \r short forms); neither escapes
 * U+007F or "/".
 */
export function canonicalJSON(v) {
  if (v === null) return "null";
  switch (typeof v) {
    case "boolean": return v ? "true" : "false";
    case "number":
      if (!Number.isFinite(v)) throw new EnvelopeError("envelope: non-finite number not permitted");
      if (!Number.isInteger(v)) throw new EnvelopeError("envelope: floats are not permitted");
      if (v < I64_MIN || v > I64_MAX) throw new EnvelopeError("envelope: integer out of i64 interop range");
      return String(v);
    case "string": return JSON.stringify(v);
    case "object": {
      if (Array.isArray(v)) return "[" + v.map(canonicalJSON).join(",") + "]";
      const keys = Object.keys(v).sort(cmpUtf8);
      return "{" + keys.map((k) => JSON.stringify(k) + ":" + canonicalJSON(v[k])).join(",") + "}";
    }
    default:
      throw new EnvelopeError("envelope: unsupported value type " + typeof v);
  }
}

async function sha256hex(s) {
  const d = await crypto.subtle.digest("SHA-256", _te.encode(s));
  const b = new Uint8Array(d);
  let out = "";
  for (let i = 0; i < b.length; i++) out += b[i].toString(16).padStart(2, "0");
  return out;
}

function parseKey(keyHex) {
  if (typeof keyHex !== "string" || !/^[0-9a-fA-F]{64}$/.test(keyHex.trim()))
    throw new EnvelopeError("envelope: authority key unavailable or malformed");
  const t = keyHex.trim();
  const bytes = new Uint8Array(32);
  for (let i = 0; i < 32; i++) bytes[i] = parseInt(t.slice(i * 2, i * 2 + 2), 16);
  return bytes;
}

async function hmacHex(keyBytes, s) {
  const k = await crypto.subtle.importKey("raw", keyBytes, { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
  const sig = await crypto.subtle.sign("HMAC", k, _te.encode(s));
  const b = new Uint8Array(sig);
  let out = "";
  for (let i = 0; i < b.length; i++) out += b[i].toString(16).padStart(2, "0");
  return out;
}

/** Constant-time hex comparison (both sides normalized to lowercase first). */
function timingSafeEqHex(a, b) {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}

/**
 * Verify one envelope. Mirrors envelope::verify exactly:
 * authenticity (sig) FIRST, then payload binding, then seal.
 * The authority sig is REQUIRED — missing/malformed/invalid sig is a hard
 * failure. There is no legacy/unsigned path for newly ingested receipts.
 * keyHex: 64-char hex authority key (Cloudflare secret at runtime).
 */
export async function verifyEnvelope(env, keyHex) {
  if (!env || typeof env !== "object" || Array.isArray(env))
    throw new EnvelopeError("envelope: must be an object");
  if (env.env !== ENV_VERSION)
    throw new EnvelopeError("envelope: unsupported env version " + JSON.stringify(env.env));
  if (typeof env.status !== "string" || !STATUS_SET.has(env.status))
    throw new EnvelopeError("envelope: bad status " + JSON.stringify(env.status));
  // AUTHENTICITY FIRST, before the payload binding.
  const key = parseKey(keyHex);
  const sig = env.sig;
  if (typeof sig !== "string")
    throw new EnvelopeError("envelope: missing sig — envelope not sealed by the authority key");
  const ci = sig.indexOf(":");
  if (ci < 0) throw new EnvelopeError('envelope: malformed sig (want "ALG:hex")');
  const alg = sig.slice(0, ci), hexsig = sig.slice(ci + 1);
  if (alg !== SIG_ALG)
    throw new EnvelopeError(`envelope: unsupported sig alg ${JSON.stringify(alg)} (only ${JSON.stringify(SIG_ALG)})`);
  if (!/^[0-9a-fA-F]+$/.test(hexsig) || hexsig.length % 2 !== 0 || hexsig.length === 0)
    throw new EnvelopeError("envelope: sig is not hex");
  const nosig = {};
  for (const k of Object.keys(env)) if (k !== "seal" && k !== "sig") nosig[k] = env[k];
  const want = await hmacHex(key, canonicalJSON(nosig));
  if (!timingSafeEqHex(want, hexsig.toLowerCase()))
    throw new EnvelopeError("envelope: sig mismatch — envelope NOT sealed by the authority key");
  const payload = env.payload;
  if (!payload || typeof payload !== "object" || Array.isArray(payload))
    throw new EnvelopeError("envelope: payload must be an object");
  if ((await sha256hex(canonicalJSON(payload))) !== env.payload_hash)
    throw new EnvelopeError("envelope: payload_hash mismatch — payload tampered");
  const noseal = {};
  for (const k of Object.keys(env)) if (k !== "seal") noseal[k] = env[k];
  if ((await sha256hex(canonicalJSON(noseal))) !== env.seal)
    throw new EnvelopeError("envelope: seal mismatch — envelope tampered");
}

/**
 * Verify a sequence: each envelope valid and prev_hash linked.
 * startPrev anchors the first envelope's expected prev_hash (GENESIS for a
 * fresh chain). Mirrors compute/envelope.py verify_chain.
 */
export async function verifyEnvelopeChain(envs, keyHex, startPrev = GENESIS) {
  if (!Array.isArray(envs)) throw new EnvelopeError("envelope: chain must be an array");
  let prev = startPrev;
  for (let i = 0; i < envs.length; i++) {
    try {
      await verifyEnvelope(envs[i], keyHex);
    } catch (e) {
      throw new EnvelopeError(`envelope ${i}: ${e.message}`);
    }
    if (envs[i].prev_hash !== prev)
      throw new EnvelopeError(`envelope ${i}: chain break — prev_hash ${JSON.stringify(envs[i].prev_hash)} != ${JSON.stringify(prev)}`);
    prev = envs[i].seal;
  }
}
