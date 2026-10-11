//! Time, ID, and hashing helpers — byte-faithful ports of the worker.js helpers.
//!
//! - `now_iso()` == `new Date().toISOString()` → "2026-10-11T05:45:30.123Z"
//!   (always millis, always `Z`)
//! - `minutes_ago(n)` == `new Date(Date.now()-n*60000).toISOString().replace("T"," ").slice(0,19)`
//!   → "2026-10-11 05:45:30" (space separator, no millis)
//! - `uid(p)` == `p + "_" + Date.now().toString(36) + Math.random().toString(36).slice(2,8)`
//! - `sha256hex(s)` == `crypto.subtle.digest("SHA-256", new TextEncoder().encode(s))` as hex

use chrono::{Duration, Utc};
use rand::Rng;
use sha2::{Digest, Sha256};

/// `new Date().toISOString()`
pub fn now_iso() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// JS `minutesAgo(n)`
pub fn minutes_ago(n: i64) -> String {
    (Utc::now() - Duration::minutes(n))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// Parse an ISO-8601 or space-separated datetime to epoch millis.
/// Mirrors the worker.js `ts()` helper used for age computations.
pub fn ts(s: &str) -> i64 {
    // Try ISO with T/Z first, then space-separated (SQLite datetime('now')).
    let iso = s.replace(' ', "T");
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&iso) {
        return dt.timestamp_millis();
    }
    // Space-separated without timezone → assume UTC.
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return ndt.and_utc().timestamp_millis();
    }
    // Date-only.
    if let Ok(nd) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return nd.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis();
    }
    0
}

const B36: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// JS `Number.prototype.toString(36)` for non-negative integers.
fn to_base36(mut n: u64) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(B36[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

/// JS `uid(p)`: `p + "_" + Date.now().toString(36) + Math.random().toString(36).slice(2, 8)`
///
/// The random suffix is 6 chars drawn from [0-9a-z], matching the shape of
/// `Math.random().toString(36).slice(2,8)`.
pub fn uid(prefix: &str) -> String {
    let millis = Utc::now().timestamp_millis().max(0) as u64;
    let mut rng = rand::thread_rng();
    let rand_part: String = (0..6)
        .map(|_| B36[rng.gen_range(0..36)] as char)
        .collect();
    format!("{}_{}{}", prefix, to_base36(millis), rand_part)
}

/// `crypto.subtle.digest("SHA-256", new TextEncoder().encode(s))` as lowercase hex.
pub fn sha256hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

/// Worker secret minting: `"wsec_" + Array.from(crypto.getRandomValues(new Uint8Array(24)),
/// x => x.toString(36)).join("").slice(0, 32)`.
///
/// Each of the 24 random bytes is rendered in base36 (1–2 chars), joined,
/// then truncated to 32 chars after the `wsec_` prefix.
pub fn mint_worker_secret() -> String {
    let mut rng = rand::thread_rng();
    let mut joined = String::new();
    for _ in 0..24 {
        let b: u8 = rng.gen();
        joined.push_str(&to_base36(b as u64));
    }
    format!("wsec_{}", &joined[..joined.len().min(32)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_iso_shape() {
        let s = now_iso();
        // "2026-10-11T05:45:30.123Z" — 24 chars, T separator, .mmm, Z suffix
        assert_eq!(s.len(), 24, "now_iso len: {s}");
        assert!(s.ends_with('Z'));
        assert_eq!(&s[10..11], "T");
        assert_eq!(&s[19..20], ".");
    }

    #[test]
    fn minutes_ago_shape() {
        let s = minutes_ago(30);
        // "2026-10-11 05:45:30" — 19 chars, space separator, no millis
        assert_eq!(s.len(), 19, "minutes_ago len: {s}");
        assert_eq!(&s[10..11], " ");
    }

    #[test]
    fn uid_shape() {
        let id = uid("rcpt");
        assert!(id.starts_with("rcpt_"), "uid prefix: {id}");
        let rest = &id[5..];
        assert!(rest.len() >= 7, "uid body len: {id}");
        assert!(rest.chars().all(|c| c.is_ascii_alphanumeric() && !c.is_ascii_uppercase()),
            "uid body is lowercase base36: {id}");
    }

    #[test]
    fn uid_unique() {
        let a = uid("x");
        let b = uid("x");
        assert_ne!(a, b);
    }

    #[test]
    fn sha256hex_known() {
        // echo -n "abc" | sha256sum
        assert_eq!(
            sha256hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn worker_secret_shape() {
        let s = mint_worker_secret();
        assert!(s.starts_with("wsec_"));
        assert_eq!(s.len(), 5 + 32, "secret len: {s}");
        assert!(s[5..].chars().all(|c| c.is_ascii_alphanumeric() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn to_base36_matches_js() {
        // (1798765432100).toString(36) in JS == "mf2abc123"-style; spot-check small values
        assert_eq!(to_base36(0), "0");
        assert_eq!(to_base36(35), "z");
        assert_eq!(to_base36(36), "10");
        assert_eq!(to_base36(1296), "100");
    }
}
