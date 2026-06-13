pub mod claude;
pub mod codex;
pub mod pi;

use crate::model::{Agent, SessionData};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct SessionRef {
    pub agent: Agent,
    pub session_id: String,
    pub path: PathBuf,
}

pub trait Adapter {
    fn agent(&self) -> Agent;
    /// Discover all session files for this agent under $HOME.
    fn discover(&self) -> anyhow::Result<Vec<SessionRef>>;
    /// Parse one session file into normalized data (non-overlapping token buckets).
    fn parse(&self, r: &SessionRef) -> anyhow::Result<SessionData>;
}

pub fn all_adapters() -> Vec<Box<dyn Adapter>> {
    vec![
        Box::new(claude::ClaudeAdapter::new()),
        Box::new(codex::CodexAdapter::new()),
        Box::new(pi::PiAdapter::new()),
    ]
}

pub fn adapter_for(agent: Agent) -> Option<Box<dyn Adapter>> {
    all_adapters().into_iter().find(|a| a.agent() == agent)
}
