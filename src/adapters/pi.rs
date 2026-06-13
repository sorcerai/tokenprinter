use super::{Adapter, SessionRef};
use crate::model::{Agent, CacheTtl, SessionData, UsageRecord};
use anyhow::Context;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

pub struct PiAdapter { root: PathBuf }

impl PiAdapter {
    pub fn new() -> Self {
        let root = dirs::home_dir().unwrap_or_default().join(".pi/agent/sessions");
        Self { root }
    }
}

fn u(v: &Value, k: &str) -> u64 { v.get(k).and_then(|x| x.as_u64()).unwrap_or(0) }

impl Adapter for PiAdapter {
    fn agent(&self) -> Agent { Agent::Pi }

    fn discover(&self) -> anyhow::Result<Vec<SessionRef>> {
        let mut out = Vec::new();
        if !self.root.exists() { return Ok(out); }
        for e in walkdir::WalkDir::new(&self.root).into_iter().flatten() {
            let p = e.path();
            if p.extension().map(|x| x=="jsonl").unwrap_or(false) {
                let sid = p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
                out.push(SessionRef { agent: Agent::Pi, session_id: sid, path: p.to_path_buf() });
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> anyhow::Result<SessionData> {
        let f = std::fs::File::open(&r.path).with_context(|| format!("open {}", r.path.display()))?;
        let mut records = Vec::new();
        let mut tools: BTreeMap<String, u32> = BTreeMap::new();
        let mut project = None;
        let mut session_id = r.session_id.clone();
        let mut first_ts: Option<DateTime<Utc>> = None;
        let mut last_ts: Option<DateTime<Utc>> = None;

        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() { continue; }
            let v: Value = match serde_json::from_str(&line) { Ok(v)=>v, Err(_)=>continue };
            if project.is_none() { project = v.get("cwd").and_then(|x| x.as_str()).map(String::from); }
            if let Some(s) = v.get("sessionId").and_then(|x| x.as_str()) { session_id = s.to_string(); }
            let ts = v.get("timestamp").and_then(|x| x.as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&Utc));
            if let Some(t) = ts { first_ts.get_or_insert(t); last_ts = Some(t); }

            let msg = match v.get("message") { Some(m)=>m, None=>continue };
            if msg.get("role").and_then(|x| x.as_str()) != Some("assistant") { continue; }

            if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
                for item in arr {
                    let t = item.get("type").and_then(|x| x.as_str()).unwrap_or("");
                    if t == "tool_use" || t == "tool_call" {
                        if let Some(n) = item.get("name").and_then(|x| x.as_str()) {
                            *tools.entry(n.to_string()).or_insert(0) += 1;
                        }
                    }
                }
            }

            if let Some(usage) = v.get("usage") {
                let model = msg.get("model").and_then(|m| m.as_str()).unwrap_or("unknown").to_string();
                let input = u(usage, "input_tokens");
                let output = u(usage, "output_tokens");
                let cache_write = u(usage, "cache_creation_input_tokens");
                let cache_read = u(usage, "cache_read_input_tokens");
                let cost = v.get("cost").and_then(|c| c.as_f64());
                // Timestamp fallback: prefer last-seen ts, then first_ts, then Utc::now.
                let rec_ts = ts
                    .or(last_ts)
                    .or(first_ts)
                    .unwrap_or_else(Utc::now);
                records.push(UsageRecord {
                    agent: Agent::Pi, provider: "multi".into(), model,
                    session_id: session_id.clone(), project: project.clone(),
                    timestamp: rec_ts,
                    input, output, cache_write, cache_read, reasoning: 0,
                    context_size: input + cache_write + cache_read,
                    cache_write_ttl: CacheTtl::FiveMin, cost,
                });
            }
        }
        let started = first_ts.unwrap_or_else(Utc::now);
        let ended = last_ts.unwrap_or(started);
        let turns = records.len() as u32;
        Ok(SessionData {
            agent: Agent::Pi, session_id, project, git_branch: None,
            started_at: started, ended_at: ended, records,
            tool_calls: tools.into_iter().collect(), turns,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::{Adapter, SessionRef};
    use crate::model::Agent;
    use std::path::PathBuf;

    #[test]
    fn parses_pi_session_with_reported_cost() {
        let a = PiAdapter::new();
        let r = SessionRef { agent: Agent::Pi, session_id: "pi-1".into(),
            path: PathBuf::from("tests/fixtures/pi_session.jsonl") };
        let s = a.parse(&r).unwrap();
        assert_eq!(s.records.len(), 2);
        assert_eq!(s.project.as_deref(), Some("/tmp/piproj"));
        assert_eq!(s.records[0].cost, Some(0.0123));
        assert_eq!(s.records[0].model, "claude-sonnet-4-6");
        assert_eq!(s.records[0].cache_read, 1000);
        // read_file x2, shell x1
        assert_eq!(s.tool_calls.iter().find(|(n,_)| n=="read_file").unwrap().1, 2);
        assert_eq!(s.tool_calls.iter().find(|(n,_)| n=="shell").unwrap().1, 1);
    }
}
