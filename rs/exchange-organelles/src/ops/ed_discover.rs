//! ed/discover — local skill discovery sidecar. Port of discover-local.py.
//! Scans workspace skills for vault-connected API capabilities and
//! registers the missing ones as DISCOVERED provider candidates via
//! /v1/discover. Listings enter as leads (usable=false); a listing is
//! never capacity.

use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;

use super::{exchange_key, expanduser, http_client, OpError};

pub const EXCHANGE: &str = "https://lucidota-exchange.cutecity.workers.dev";
pub const SKILLS_DIR: &str = "~/workspace/skills";

/// Skill dirs already represented under a different provider id.
pub fn alias_for(dir: &str) -> &str {
    match dir {
        "nvidia-nim" => "nvidia",
        _ => dir,
    }
}

#[derive(Debug, PartialEq)]
pub struct Skill {
    pub dir: String,
    pub title: String,
    pub desc: String,
    pub creds: Vec<String>,
    pub hosts: Vec<String>,
}

pub fn parse_skill(path: &str, text: &str) -> Skill {
    let dir = std::path::Path::new(path)
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    let name_re = Regex::new(r#"(?m)^name:\s*"([^"]+)""#).unwrap();
    let desc_re = Regex::new(r#"(?m)^description:\s*"([^"]+)""#).unwrap();
    let cred_re = Regex::new(r"custom\.([a-z0-9_-]+)").unwrap();
    let host_re = Regex::new(r"https?://([a-z0-9_.-]+)").unwrap();
    let title = name_re
        .captures(text)
        .map(|c| c[1].to_string())
        .unwrap_or_else(|| dir.clone());
    let desc = desc_re
        .captures(text)
        .map(|c| c[1].to_string())
        .unwrap_or_default();
    let mut creds: Vec<String> = cred_re
        .captures_iter(text)
        .map(|c| c[1].to_string())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    creds.sort();
    let mut hosts: Vec<String> = host_re
        .captures_iter(text)
        .map(|c| c[1].to_string())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    hosts.sort();
    Skill {
        dir,
        title,
        desc,
        creds,
        hosts,
    }
}

pub fn discover_notes(s: &Skill) -> String {
    let creds = s.creds.iter().map(|c| format!("custom.{c}")).collect::<Vec<_>>().join(", ");
    let hosts = if s.hosts.is_empty() {
        "n/a".to_string()
    } else {
        s.hosts.iter().take(4).cloned().collect::<Vec<_>>().join(", ")
    };
    format!(
        "Local skill '{}': {} Vault credential(s): {}. Hosts: {}.",
        s.title, s.desc, creds, hosts
    )
}

fn xchg(
    client: &reqwest::blocking::Client,
    key: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, OpError> {
    let mut req = client
        .request(method, format!("{EXCHANGE}{path}"))
        .header("Authorization", format!("Bearer {key}"))
        .header("User-Agent", "lucidota-exchange-discovery/1.0");
    if let Some(b) = body {
        req = req
            .header("Content-Type", "application/json")
            .body(serde_json::to_string(b).unwrap());
    }
    let resp = req
        .send()
        .map_err(|e| OpError::failed(format!("{path}: {e}")))?;
    resp.json::<Value>()
        .map_err(|e| OpError::failed(format!("{path} decode: {e}")))
}

pub fn run(_input: Value) -> Result<Value, OpError> {
    let key = exchange_key()?;
    let client = http_client(30);

    let mut existing: HashSet<String> = HashSet::new();
    match xchg(&client, &key, reqwest::Method::GET, "/v1/resources", None) {
        Ok(res) => {
            for r in res.get("resources").and_then(|r| r.as_array()).into_iter().flatten() {
                if let Some(p) = r.get("provider").and_then(|p| p.as_str()) {
                    existing.insert(p.to_string());
                }
            }
        }
        Err(e) => return Err(OpError::failed(format!("list failed: {e}"))),
    }

    let skills_root = expanduser(SKILLS_DIR);
    let mut mds: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&skills_root) {
        for entry in rd.flatten() {
            let md = entry.path().join("SKILL.md");
            if md.is_file() {
                mds.push(md.to_string_lossy().into_owned());
            }
        }
    }
    mds.sort();

    let mut added: Vec<String> = Vec::new();
    for md in &mds {
        let text = std::fs::read_to_string(md).unwrap_or_default();
        let s = parse_skill(md, &text);
        if s.creds.is_empty() {
            continue; // no vault credential -> nothing to bridge
        }
        let alias = alias_for(&s.dir);
        if existing.contains(alias) {
            continue;
        }
        let body = serde_json::json!({
            "provider_id": alias,
            "provider_name": s.title,
            "resource": false,
            "notes": discover_notes(&s),
            "evidence_source": "local-skill-scan",
        });
        match xchg(&client, &key, reqwest::Method::POST, "/v1/discover", Some(&body)) {
            Ok(r) if r.get("error").is_none() => {
                added.push(alias.to_string());
                existing.insert(alias.to_string());
            }
            _ => { /* warn-and-continue, like the Python sidecar */ }
        }
    }

    Ok(serde_json::json!({"added": added, "scanned": mds.len()}))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"---
name: "deepseek"
description: "Use Deepseek when the user asks for Deepseek or this provider's API."
---
Some text mentioning custom.deepseek and https://api.deepseek.com/v1 and
a second host http://localhost:8080/x. Also custom.deepseek again.
"#;

    #[test]
    fn parse_skill_fields() {
        let s = parse_skill("/home/hatch/workspace/skills/deepseek/SKILL.md", SAMPLE);
        assert_eq!(s.dir, "deepseek");
        assert_eq!(s.title, "deepseek");
        assert!(s.desc.starts_with("Use Deepseek"));
        assert_eq!(s.creds, vec!["deepseek"]);
        assert_eq!(s.hosts, vec!["api.deepseek.com", "localhost"]);
    }

    #[test]
    fn aliases() {
        assert_eq!(alias_for("nvidia-nim"), "nvidia");
        assert_eq!(alias_for("deepseek"), "deepseek");
    }

    #[test]
    fn notes_shape() {
        let s = Skill {
            dir: "d".into(),
            title: "T".into(),
            desc: "D".into(),
            creds: vec!["a".into(), "b".into()],
            hosts: vec![],
        };
        let n = discover_notes(&s);
        assert!(n.contains("custom.a, custom.b"));
        assert!(n.contains("Hosts: n/a"));
    }
}
