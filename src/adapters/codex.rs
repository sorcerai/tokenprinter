use super::{Adapter, SessionRef};
use crate::model::{Agent, CacheTtl, SessionData, UsageRecord};
use anyhow::Context;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

pub struct CodexAdapter { root: PathBuf }

impl CodexAdapter {
    pub fn new() -> Self {
        let root = dirs::home_dir().unwrap_or_default().join(".codex/sessions");
        Self { root }
    }
}

fn u(v: &Value, k: &str) -> u64 { v.get(k).and_then(|x| x.as_u64()).unwrap_or(0) }

impl Adapter for CodexAdapter {
    fn agent(&self) -> Agent { Agent::Codex }

    fn discover(&self) -> anyhow::Result<Vec<SessionRef>> {
        let mut out = Vec::new();
        if !self.root.exists() { return Ok(out); }
        for e in walkdir::WalkDir::new(&self.root).into_iter().flatten() {
            let p = e.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                // session id is the uuid tail of the filename
                let sid = name.trim_start_matches("rollout-").trim_end_matches(".jsonl").to_string();
                out.push(SessionRef { agent: Agent::Codex, session_id: sid, path: p.to_path_buf() });
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> anyhow::Result<SessionData> {
        let f = std::fs::File::open(&r.path).with_context(|| format!("open {}", r.path.display()))?;
        let mut project = None;
        let mut model = "gpt-5.5".to_string();
        let mut session_id = r.session_id.clone();
        let mut tools: BTreeMap<String, u32> = BTreeMap::new();
        let mut first_ts: Option<DateTime<Utc>> = None;
        let mut last_ts: Option<DateTime<Utc>> = None;
        let mut last_total: Option<(u64,u64,u64,u64)> = None; // (input_total, cached, output, reasoning)

        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() { continue; }
            let v: Value = match serde_json::from_str(&line) { Ok(v)=>v, Err(_)=>continue };
            let ts = v.get("timestamp").and_then(|x| x.as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&Utc));
            if let Some(t) = ts { first_ts.get_or_insert(t); last_ts = Some(t); }

            let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
            let payload = v.get("payload");
            match (ty, payload) {
                ("session_meta", Some(p)) => {
                    if project.is_none() { project = p.get("cwd").and_then(|x| x.as_str()).map(String::from); }
                    if let Some(m) = p.get("model").and_then(|x| x.as_str()) { model = m.to_string(); }
                    if let Some(id) = p.get("id").and_then(|x| x.as_str()) { session_id = id.to_string(); }
                }
                ("event_msg", Some(p)) => {
                    match p.get("type").and_then(|x| x.as_str()).unwrap_or("") {
                        "function_call" => {
                            if let Some(n) = p.get("name").and_then(|x| x.as_str()) {
                                *tools.entry(n.to_string()).or_insert(0) += 1;
                            }
                        }
                        "token_count" => {
                            if let Some(info) = p.get("info") {
                                if let Some(tot) = info.get("total_token_usage") {
                                    last_total = Some((
                                        u(tot, "input_tokens"),
                                        u(tot, "cached_input_tokens"),
                                        u(tot, "output_tokens"),
                                        u(tot, "reasoning_output_tokens"),
                                    ));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        let mut records = Vec::new();
        if let Some((input_total, cached, output, reasoning)) = last_total {
            let input = input_total.saturating_sub(cached);
            let cache_read = cached;
            records.push(UsageRecord {
                agent: Agent::Codex, provider: "openai".into(), model: model.clone(),
                session_id: session_id.clone(), project: project.clone(),
                timestamp: last_ts.unwrap_or_else(Utc::now),
                input, output, cache_write: 0, cache_read, reasoning,
                context_size: input + cache_read,
                cache_write_ttl: CacheTtl::FiveMin, cost: None,
            });
        }
        let started = first_ts.unwrap_or_else(Utc::now);
        let ended = last_ts.unwrap_or(started);
        let turns = tools.values().sum::<u32>().max(records.len() as u32);
        Ok(SessionData {
            agent: Agent::Codex, session_id, project, git_branch: None,
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
    fn parses_codex_session_with_non_overlapping_buckets() {
        let a = CodexAdapter::new();
        let r = SessionRef { agent: Agent::Codex, session_id: "cdx-1".into(),
            path: PathBuf::from("tests/fixtures/codex_session.jsonl") };
        let s = a.parse(&r).unwrap();
        assert_eq!(s.agent, Agent::Codex);
        assert_eq!(s.project.as_deref(), Some("/tmp/codexproj"));
        assert_eq!(s.records.len(), 1); // cumulative -> single total record
        let rec = &s.records[0];
        assert_eq!(rec.model, "gpt-5.5");
        // last cumulative: input_total 3000, cached 1500 -> input 1500, cache_read 1500
        assert_eq!(rec.input, 1500);
        assert_eq!(rec.cache_read, 1500);
        assert_eq!(rec.cache_write, 0);
        assert_eq!(rec.output, 600);
        assert_eq!(rec.reasoning, 120);
        // shell called twice
        assert_eq!(s.tool_calls.iter().find(|(n,_)| n=="shell").unwrap().1, 2);
    }
}
