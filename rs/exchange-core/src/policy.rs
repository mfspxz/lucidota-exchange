//! Policy helpers: policyVersion, taxonomyOf, costOf.
//!
//! policyVersion is a hash of the canonical policy set (first 16 hex
//! chars), NOT a version column — every receipt/lease records exactly
//! which rules authorized it.

use crate::db::{Db, DbError, RowExt};
use crate::util::sha256hex;
use serde_json::Value;

/// `policyVersion(db)`: `sha256hex(JSON.stringify(policies.map(p => [p.id, p.rule, p.effect]))).slice(0,16)`
pub async fn policy_version(db: &dyn Db) -> Result<String, DbError> {
    let rows = db
        .query_all("SELECT id,rule,effect FROM policies ORDER BY id", &[])
        .await?;
    let canon: Vec<Vec<Value>> = rows
        .iter()
        .map(|p| {
            vec![
                Value::from(p.text_or("id", "")),
                Value::from(p.text_or("rule", "")),
                Value::from(p.text_or("effect", "")),
            ]
        })
        .collect();
    Ok(sha256hex(&serde_json::to_string(&canon).unwrap_or_default())[..16].to_string())
}

/// Failure taxonomy — distinct failures are never collapsed into "failed".
pub fn taxonomy_of(error_class: &str) -> &'static str {
    match error_class {
        "RATE_LIMITED" => "QUOTA_FAILURE",
        "TIMEOUT" | "NETWORK" | "SERVER_ERROR" | "UNKNOWN" => "RETRYABLE_FAILURE",
        "BAD_RESPONSE" => "PERMANENT_FAILURE",
        "POLICY_BLOCKED" => "POLICY_FAILURE",
        "AUTH_ERROR" => "AUTH_FAILURE",
        "NO_ADAPTER" => "CAPABILITY_FAILURE",
        "VERIFICATION_FAILED" => "VERIFICATION_FAILURE",
        _ => "RETRYABLE_FAILURE",
    }
}

/// `costOf(r)`: v0 — free resources cost 0; paid carry config.usd_per_unit.
pub fn cost_of(resource: &crate::db::Row) -> f64 {
    if resource.text_or("cost_class", "") == "free" {
        return 0.0;
    }
    let cfg: Value = serde_json::from_str(&resource.text_or("config", "{}")).unwrap_or(Value::Null);
    cfg.get("usd_per_unit").and_then(|v| v.as_f64()).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taxonomy_mapping() {
        assert_eq!(taxonomy_of("RATE_LIMITED"), "QUOTA_FAILURE");
        assert_eq!(taxonomy_of("TIMEOUT"), "RETRYABLE_FAILURE");
        assert_eq!(taxonomy_of("BAD_RESPONSE"), "PERMANENT_FAILURE");
        assert_eq!(taxonomy_of("POLICY_BLOCKED"), "POLICY_FAILURE");
        assert_eq!(taxonomy_of("AUTH_ERROR"), "AUTH_FAILURE");
        assert_eq!(taxonomy_of("NO_ADAPTER"), "CAPABILITY_FAILURE");
        assert_eq!(taxonomy_of("VERIFICATION_FAILED"), "VERIFICATION_FAILURE");
        assert_eq!(taxonomy_of("SOMETHING_ELSE"), "RETRYABLE_FAILURE");
    }
}
