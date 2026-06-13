use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent { Claude, Codex, Pi, Agy }

impl Agent {
    pub fn label(&self) -> &'static str {
        match self { Agent::Claude => "Claude Code", Agent::Codex => "Codex",
                     Agent::Pi => "pi", Agent::Agy => "agy" }
    }
    pub fn provider(&self) -> &'static str {
        match self { Agent::Claude => "anthropic", Agent::Codex => "openai",
                     Agent::Pi => "multi", Agent::Agy => "google" }
    }
    pub fn slug(&self) -> &'static str {
        match self { Agent::Claude => "claude", Agent::Codex => "codex",
                     Agent::Pi => "pi", Agent::Agy => "agy" }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTtl { FiveMin, OneHour }

#[derive(Debug, Clone)]
pub struct UsageRecord {
    pub agent: Agent,
    pub provider: String,
    pub model: String,
    pub session_id: String,
    pub project: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub input: u64,        // NON-cached input
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub reasoning: u64,
    pub context_size: u64, // input + cache_read + cache_write, for tier selection
    pub cache_write_ttl: CacheTtl,
    pub cost: Option<f64>, // tool-reported, if present
}

impl UsageRecord {
    pub fn zeroed(agent: Agent, model: &str) -> Self {
        UsageRecord {
            agent, provider: agent.provider().to_string(), model: model.to_string(),
            session_id: String::new(), project: None, timestamp: Utc::now(),
            input: 0, output: 0, cache_write: 0, cache_read: 0, reasoning: 0,
            context_size: 0, cache_write_ttl: CacheTtl::FiveMin, cost: None,
        }
    }
    pub fn total_tokens(&self) -> u64 {
        self.input + self.output + self.cache_write + self.cache_read
    }
}

#[derive(Debug, Clone)]
pub struct SessionData {
    pub agent: Agent,
    pub session_id: String,
    pub project: Option<String>,
    pub git_branch: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub records: Vec<UsageRecord>,
    pub tool_calls: Vec<(String, u32)>,
    pub turns: u32,
}

// ---- Receipt (assembled, ready to render) ----
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope { Session, Daily, OnDemand }

#[derive(Debug, Clone)]
pub struct ModelLine {
    pub model: String,
    pub input: u64, pub output: u64, pub cache_write: u64, pub cache_read: u64,
    pub cost: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct GitStats { pub files_changed: u32, pub added: u32, pub removed: u32, pub commits: u32 }

#[derive(Debug, Clone, Default)]
pub struct BeadsStats { pub opened: Vec<String>, pub closed: Vec<String> }

#[derive(Debug, Clone)]
pub struct Receipt {
    pub scope: Scope,
    pub agent: Agent,
    pub location: String,
    pub session_name: String,
    pub project: Option<String>,
    pub git_branch: Option<String>,
    pub when: DateTime<Utc>,
    pub duration_secs: i64,
    pub per_model: Vec<ModelLine>,
    pub total_tokens: u64,
    pub total_cost: Option<f64>,
    pub cache_saved_tokens: u64,
    pub cache_saved_usd: Option<f64>,
    pub cache_hit_rate: f64,
    pub burn_rate_per_hr: Option<f64>,
    pub tools: Vec<(String, u32)>,
    pub git: GitStats,
    pub beads: BeadsStats,
    pub sparkline: Vec<u8>, // 0..=7 bucket heights
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn agent_label_and_provider() {
        assert_eq!(Agent::Claude.label(), "Claude Code");
        assert_eq!(Agent::Claude.provider(), "anthropic");
        assert_eq!(Agent::Codex.provider(), "openai");
        assert_eq!(Agent::Pi.label(), "pi");
    }
    #[test]
    fn usage_totals_sum_buckets() {
        let r = UsageRecord::zeroed(Agent::Claude, "claude-opus-4-8");
        let mut r2 = r.clone();
        r2.input = 100; r2.output = 200; r2.cache_write = 10; r2.cache_read = 5;
        assert_eq!(r2.total_tokens(), 315);
    }
}
