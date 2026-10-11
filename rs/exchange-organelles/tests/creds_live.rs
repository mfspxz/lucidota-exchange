//! Live check of the authd surrogate flow (ignored by default: needs the
//! local authd socket). Run: cargo test -- --ignored creds_live
//! Asserts only the surrogate SHAPE (hsurr: prefix) — the value is never
//! printed, logged, or persisted.

use exchange_organelles::creds;

#[test]
#[ignore]
fn creds_live_cloudflare_surrogate_shape() {
    // placement check only; the token value never leaves this scope
    let entry = creds::dynamic_credential_entry("custom.cloudflare", "access_token");
    match entry {
        Ok(_) => {
            // entry parsed and placement supported; surrogate starts hsurr:
            // (verified inside dynamic_credential_entry — it errors otherwise)
        }
        Err(e) => panic!("authd surrogate fetch failed: {e}"),
    }
}

#[test]
#[ignore]
fn creds_live_missing_credential_errors() {
    let r = creds::dynamic_credential_entry("custom.does-not-exist-xyz", "access_token");
    assert!(r.is_err());
}
