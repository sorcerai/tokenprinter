use super::{Adapter, SessionRef};
use crate::model::{Agent, CacheTtl, SessionData, UsageRecord};
use anyhow::Context;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

pub struct ClaudeAdapter { root: PathBuf }

impl Default for ClaudeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeAdapter {
    pub fn new() -> Self {
        let root = dirs::home_dir().unwrap_or_default().join(".claude/projects");
        Self { root }
    }
}

fn u(v: &Value, k: &str) -> u64 { v.get(k).and_then(|x| x.as_u64()).unwrap_or(0) }

impl Adapter for ClaudeAdapter {
    fn agent(&self) -> Agent { Agent::Claude }

    fn discover(&self) -> anyhow::Result<Vec<SessionRef>> {
        let mut out = Vec::new();
        if !self.root.exists() { return Ok(out); }
        for e in walkdir::WalkDir::new(&self.root).into_iter().flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
                let sid = p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
                out.push(SessionRef { agent: Agent::Claude, session_id: sid, path: p.to_path_buf() });
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> anyhow::Result<SessionData> {
        let f = std::fs::File::open(&r.path)
            .with_context(|| format!("open {}", r.path.display()))?;
        let mut records = Vec::new();
        let mut tools: BTreeMap<String, u32> = BTreeMap::new();
        let mut project = None;
        let mut branch = None;
        let mut session_id = r.session_id.clone();
        let mut first_ts: Option<DateTime<Utc>> = None;
        let mut last_ts: Option<DateTime<Utc>> = None;

        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() { continue; }
            let v: Value = match serde_json::from_str(&line) { Ok(v) => v, Err(_) => continue };

            if project.is_none() { project = v.get("cwd").and_then(|x| x.as_str()).map(String::from); }
            if branch.is_none() { branch = v.get("gitBranch").and_then(|x| x.as_str()).map(String::from); }
            if let Some(s) = v.get("sessionId").and_then(|x| x.as_str()) { session_id = s.to_string(); }

            let ts = v.get("timestamp").and_then(|x| x.as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc));
            if let Some(t) = ts { first_ts.get_or_insert(t); last_ts = Some(t); }

            let msg = match v.get("message") { Some(m) => m, None => continue };
            // tool_use counts (any message that has content array)
            if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
                for item in arr {
                    if item.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                            *tools.entry(name.to_string()).or_insert(0) += 1;
                        }
                    }
                }
            }
            // usage -> one record per assistant turn that has usage
            if let Some(usage) = msg.get("usage") {
                let model = msg.get("model").and_then(|m| m.as_str()).unwrap_or("unknown").to_string();
                let input = u(usage, "input_tokens");
                let cache_write = u(usage, "cache_creation_input_tokens");
                let cache_read = u(usage, "cache_read_input_tokens");
                let output = u(usage, "output_tokens");
                // Timestamp fallback: prefer last-seen ts, then first_ts, then Utc::now.
                let rec_ts = ts
                    .or(last_ts)
                    .or(first_ts)
                    .unwrap_or_else(Utc::now);
                records.push(UsageRecord {
                    agent: Agent::Claude, provider: "anthropic".into(), model,
                    session_id: session_id.clone(), project: project.clone(),
                    timestamp: rec_ts,
                    input, output, cache_write, cache_read, reasoning: 0,
                    context_size: input + cache_write + cache_read,
                    cache_write_ttl: CacheTtl::FiveMin, cost: None,
                });
            }
        }

        let started = first_ts.unwrap_or_else(Utc::now);
        let ended = last_ts.unwrap_or(started);
        let turns = records.len() as u32;
        Ok(SessionData {
            agent: Agent::Claude, session_id, project, git_branch: branch,
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

    fn fixture_ref() -> SessionRef {
        SessionRef { agent: Agent::Claude, session_id: "sess-abc".into(),
            path: PathBuf::from("tests/fixtures/claude_session.jsonl") }
    }

    #[test]
    fn parses_claude_session() {
        let a = ClaudeAdapter::new();
        let s = a.parse(&fixture_ref()).unwrap();
        assert_eq!(s.agent, Agent::Claude);
        assert_eq!(s.session_id, "sess-abc");
        assert_eq!(s.project.as_deref(), Some("/tmp/proj"));
        assert_eq!(s.git_branch.as_deref(), Some("main"));
        assert_eq!(s.records.len(), 2); // two assistant turns
        assert_eq!(s.turns, 2);

        let r0 = &s.records[0];
        assert_eq!(r0.model, "claude-opus-4-8[1m]");
        assert_eq!(r0.input, 100);
        assert_eq!(r0.cache_write, 2000);
        assert_eq!(r0.cache_read, 5000);
        assert_eq!(r0.output, 300);
        assert_eq!(r0.context_size, 100 + 2000 + 5000);

        // tool counts aggregated across turns: Edit x2, Bash x1
        let edit = s.tool_calls.iter().find(|(n,_)| n=="Edit").unwrap().1;
        let bash = s.tool_calls.iter().find(|(n,_)| n=="Bash").unwrap().1;
        assert_eq!(edit, 2);
        assert_eq!(bash, 1);
    }
}
