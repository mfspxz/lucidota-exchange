//! Voting (protocol amendments A1/A2/A3) — port of worker.js.
//!
//! Voting is a governor composition pattern: each vote is one ordinary
//! delegated job; the quorum tally is a deterministic reduction over N
//! receipts. Mechanical vote extraction — no LLM judging.

use crate::db::{Db, DbError, RowExt, Val};
use crate::policy::policy_version;
use crate::util::{now_iso, uid};
use serde_json::{json, Value};

pub struct BallotPolicy {
    pub voters: Vec<String>,
    pub quorum_rule: String,
    pub deadline_s: i64,
    pub tie_policy: String,
}

pub async fn get_ballot_policy(db: &dyn Db) -> Result<BallotPolicy, DbError> {
    let p = db
        .query_one("SELECT * FROM ballot_policy WHERE id='default'", &[])
        .await?
        .unwrap_or_default();
    let voters: Vec<String> =
        serde_json::from_str(&p.text_or("voters", "[]")).unwrap_or_default();
    Ok(BallotPolicy {
        voters,
        quorum_rule: p.text_or("quorum_rule", "majority"),
        deadline_s: p.i64_or("deadline_s", 600),
        tie_policy: p.text_or("tie_policy", "unknown"),
    })
}

pub fn ballot_prompt(btype: &str, proposition: &str) -> String {
    if btype == "verdict" {
        format!(
            "{}\n\nEvaluate the proposition above. Think step by step, then give your\nfinal verdict on its own line at the very end, exactly one word:\nAUTHORIZED or UNKNOWN or DENIED.\n\nFinal verdict:",
            proposition
        )
    } else {
        format!("{}\n\nAnswer with the final answer only, no explanation:", proposition)
    }
}

pub fn model_slug(m: &str) -> String {
    m.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(40)
        .collect()
}

/// Mechanical vote extraction — deterministic from receipts alone.
pub fn extract_vote(btype: &str, output: Option<&Value>) -> String {
    let content = output
        .and_then(|o| {
            o.get("content")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    o.get("reasoning_content")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
        })
        .unwrap_or_default();
    if btype == "verdict" {
        // REDTEAM-2: take the LAST keyword in the tail (was first).
        let tail: String = content.chars().rev().take(300).collect::<String>().chars().rev().collect();
        let mut last: Option<&str> = None;
        // Find all whole-word AUTHORIZED|UNKNOWN|DENIED in the tail.
        let words = ["AUTHORIZED", "UNKNOWN", "DENIED"];
        let mut idx = 0;
        let bytes = tail.as_bytes();
        while idx < bytes.len() {
            let mut matched = None;
            for w in &words {
                if tail[idx..].starts_with(w) {
                    let before_ok = idx == 0 || !is_word_char(tail.as_bytes()[idx - 1] as char);
                    let after = idx + w.len();
                    let after_ok = after >= tail.len() || !is_word_char(tail.as_bytes()[after] as char);
                    if before_ok && after_ok {
                        matched = Some(*w);
                        break;
                    }
                }
            }
            if let Some(w) = matched {
                last = Some(w);
                idx += w.len();
            } else {
                idx += 1;
            }
        }
        return last.unwrap_or("INVALID").to_string();
    }
    let norm = content
        .trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let norm = norm.trim_end_matches(|c| matches!(c, '.' | ',' | ';' | ':' | '!' | '?')).to_string();
    if norm.is_empty() {
        "INVALID".to_string()
    } else {
        norm
    }
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

pub fn verdict_of_vote(btype: &str, vote: &str) -> Option<i64> {
    if btype != "verdict" {
        return None;
    }
    match vote {
        "AUTHORIZED" => Some(1),
        "DENIED" => Some(-1),
        "UNKNOWN" => Some(0),
        _ => None,
    }
}

/// tallyBallot: deterministic quorum tally over receipts.
pub async fn tally_ballot(db: &dyn Db, ballot: &crate::db::Row) -> Result<Value, DbError> {
    let ballot_id = ballot.text_or("id", "");
    let ballot_type = ballot.text_or("ballot_type", "");
    let voters: Vec<String> =
        serde_json::from_str(&ballot.text_or("voters", "[]")).unwrap_or_default();
    // One vote per job: latest receipt per job_id. The ballot decision
    // receipt is excluded (job_id LIKE 'ballot:%' — REDTEAM-2 phantom vote).
    let receipts = db
        .query_all(
            "SELECT r.id, r.job_id, r.decision, r.worker_outcome FROM receipts r
             JOIN (SELECT job_id, MAX(created_at) AS mc FROM receipts
                   WHERE ballot_id=? AND job_id NOT LIKE 'ballot:%' GROUP BY job_id) latest
               ON r.job_id = latest.job_id AND r.created_at = latest.mc
             WHERE r.ballot_id=? AND r.job_id NOT LIKE 'ballot:%'",
            &[Val::from(ballot_id.clone()), Val::from(ballot_id.clone())],
        )
        .await?;
    let mut votes: Vec<Value> = Vec::new();
    for r in receipts {
        let dec: Value = serde_json::from_str(&r.text_or("decision", "{}")).unwrap_or(json!({}));
        let vote = extract_vote(&ballot_type, dec.get("output"));
        votes.push(json!({
            "receipt_id": r.text_or("id", ""),
            "job_id": r.text_or("job_id", ""),
            "model": dec.get("vote_model").and_then(|v| v.as_str()).unwrap_or("unknown"),
            "vote": vote.clone(),
            "valid": vote != "INVALID",
            "worker_outcome": r.text_or("worker_outcome", ""),
        }));
    }
    let valid: Vec<&Value> = votes.iter().filter(|v| v["valid"].as_bool().unwrap_or(false)).collect();
    let mut counts: serde_json::Map<String, Value> = serde_json::Map::new();
    for v in &valid {
        let k = v["vote"].as_str().unwrap_or("").to_string();
        let n = counts.get(&k).and_then(|x| x.as_i64()).unwrap_or(0);
        counts.insert(k, Value::from(n + 1));
    }
    let quorum = voters.len() as i64 / 2 + 1;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let deadline_ms = crate::util::ts(&ballot.text_or("deadline_at", ""));
    let expired = now_ms > deadline_ms;
    let mut state = ballot.text_or("state", "");
    let mut verdict: Option<String> = if ballot.text_or("verdict", "").is_empty() {
        None
    } else {
        Some(ballot.text_or("verdict", ""))
    };
    if state == "open" {
        let mut sorted: Vec<(String, i64)> = counts
            .iter()
            .map(|(k, v)| (k.clone(), v.as_i64().unwrap_or(0)))
            .collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1));
        let top = sorted.first().cloned();
        if let Some((top_vote, top_n)) = top {
            if top_n > valid.len() as i64 / 2 && valid.len() as i64 >= quorum {
                state = "decided".to_string();
                verdict = Some(top_vote);
            }
        }
        if state == "open" && expired {
            if valid.len() as i64 >= quorum {
                state = "inconclusive".to_string();
                verdict = Some("UNKNOWN".to_string());
            } else {
                state = "expired".to_string();
                verdict = None;
            }
        } else if state == "open"
            && valid.len() as i64 >= quorum
            && sorted.len() > 1
            && sorted[0].1 == sorted[1].1
        {
            // Tied at quorum, deadline not passed — keep waiting.
        }
    }
    let tally = json!({
        "votes": votes, "counts": counts, "valid": valid.len(),
        "quorum": quorum, "voters": voters.len(), "expired": expired,
        "correlation_domain": ["nvidia-nim"],
    });
    if state != ballot.text_or("state", "") {
        db.exec(
            "UPDATE ballots SET state=?, verdict=?, tally=? WHERE id=?",
            &[
                Val::from(state.clone()),
                verdict.clone().map(Val::from).unwrap_or(Val::Null),
                Val::from(tally.to_string()),
                Val::from(ballot_id.clone()),
            ],
        )
        .await?;
        db.exec(
            "INSERT INTO receipts (id,job_id,result,policy_version,decision,errors,ballot_id) VALUES (?,?,?,?,?,?,?)",
            &[
                Val::from(uid("rcpt")),
                Val::from(format!("ballot:{}", ballot_id)),
                Val::from(state.clone()),
                Val::from(policy_version(db).await?),
                Val::from(
                    json!({ "ballot": ballot_id, "ballot_type": ballot_type,
                        "proposition": ballot.text_or("proposition", ""),
                        "tally": tally, "verdict": verdict }).to_string(),
                ),
                Val::from(format!(
                    "ballot {}: {}{}",
                    ballot_id,
                    state,
                    verdict.as_ref().map(|v| format!(" — {}", v)).unwrap_or_default()
                )),
                Val::from(ballot_id.clone()),
            ],
        )
        .await?;
    }
    Ok(json!({
        "ballot_id": ballot_id, "ballot_type": ballot_type,
        "proposition": ballot.text_or("proposition", ""),
        "state": state, "verdict": verdict,
        "tally": tally,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_verdict_takes_last() {
        // REDTEAM-2: LAST keyword in tail wins.
        let out = json!({ "content": "First AUTHORIZED then on reflection Final verdict:\nDENIED" });
        assert_eq!(extract_vote("verdict", Some(&out)), "DENIED");
    }

    #[test]
    fn extract_verdict_invalid() {
        let out = json!({ "content": "no keywords here" });
        assert_eq!(extract_vote("verdict", Some(&out)), "INVALID");
    }

    #[test]
    fn extract_answer_normalizes() {
        let out = json!({ "content": "  Paris, FRANCE.  " });
        assert_eq!(extract_vote("answer", Some(&out)), "paris, france");
    }

    #[test]
    fn model_slug_sanitizes() {
        assert_eq!(model_slug("openai/gpt-oss-20b"), "openai_gpt_oss_20b");
        assert!(model_slug(&"x".repeat(100)).len() <= 40);
    }

    #[test]
    fn verdict_of_vote_map() {
        assert_eq!(verdict_of_vote("verdict", "AUTHORIZED"), Some(1));
        assert_eq!(verdict_of_vote("verdict", "DENIED"), Some(-1));
        assert_eq!(verdict_of_vote("verdict", "UNKNOWN"), Some(0));
        assert_eq!(verdict_of_vote("verdict", "INVALID"), None);
        assert_eq!(verdict_of_vote("answer", "AUTHORIZED"), None);
    }

    #[test]
    fn ballot_prompt_shapes() {
        let p = ballot_prompt("verdict", "Is this ok?");
        assert!(p.contains("Final verdict:"));
        let p2 = ballot_prompt("answer", "What is 2+2?");
        assert!(p2.contains("final answer only"));
    }
}
