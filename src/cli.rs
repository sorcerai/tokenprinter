use crate::adapters::{adapter_for, all_adapters, SessionRef};
use crate::assemble::assemble_session;
use crate::config::Config;
use crate::enrich::{beads::beads_stats, git::{current_branch, git_stats}};
use crate::model::{Agent, GitStats, BeadsStats};
use crate::pricing::PriceTable;
use crate::render::{render_bytes, render_text};
use crate::transport::{send, Mode};
use anyhow::{anyhow, Context};
use clap::{Parser, Subcommand};
use std::path::Path;

#[derive(Parser)]
#[command(name="tokenprinter", version, about="Print AI agent token receipts")]
struct Cli { #[command(subcommand)] cmd: Cmd }

#[derive(Subcommand)]
enum Cmd {
    Print {
        #[arg(long, default_value="claude")] agent: String,
        #[arg(long)] session: Option<String>,
        #[arg(long)] last: bool,
        #[arg(long)] preview: bool,
    },
    Daily { #[arg(long)] date: Option<String>, #[arg(long)] preview: bool },
    Doctor,
}

fn agent_from_str(s: &str) -> anyhow::Result<Agent> {
    match s { "claude"=>Ok(Agent::Claude), "codex"=>Ok(Agent::Codex),
              "pi"=>Ok(Agent::Pi), "agy"=>Ok(Agent::Agy),
              _=>Err(anyhow!("unknown agent '{s}' (claude|codex|pi|agy)")) }
}

/// Pick a session: explicit id match, else the most-recently-modified file, else last in list.
pub fn pick_session<'a>(refs: &'a [SessionRef], id: Option<&str>) -> anyhow::Result<&'a SessionRef> {
    if let Some(id) = id {
        return refs.iter().find(|r| r.session_id == id)
            .ok_or_else(|| anyhow!("session '{id}' not found"));
    }
    if refs.is_empty() { return Err(anyhow!("no sessions found")); }
    let newest = refs.iter().max_by_key(|r| {
        std::fs::metadata(&r.path).and_then(|m| m.modified()).ok()
    });
    Ok(newest.unwrap_or(&refs[refs.len()-1]))
}

pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load();
    let prices = load_prices();
    match cli.cmd {
        Cmd::Print { agent, session, last: _, preview } => {
            let ag = agent_from_str(&agent)?;
            let adapter = adapter_for(ag).ok_or_else(|| anyhow!("agent {agent} not supported in phase 1"))?;
            let refs = adapter.discover()?;
            let chosen = pick_session(&refs, session.as_deref())?.clone();
            let sd = adapter.parse(&chosen)?;

            let (git, beads, branch) = enrich(&sd);
            let mut receipt = assemble_session(&sd, &prices, &cfg.location, git, beads);
            if receipt.git_branch.is_none() { receipt.git_branch = branch; }

            if preview {
                print!("{}", render_text(&receipt));
            } else {
                send(&render_bytes(&receipt), Mode::parse(&cfg.transport), &cfg.queue_name)?;
                eprintln!("printed {} receipt for {}", ag.label(), receipt.session_name);
            }
        }
        Cmd::Daily { date, preview } => {
            let receipts = daily_receipts(&prices, &cfg, date.as_deref())?;
            if receipts.is_empty() { eprintln!("no sessions for that day"); return Ok(()); }
            for r in &receipts {
                if preview { print!("{}", render_text(r)); }
                else { send(&render_bytes(r), Mode::parse(&cfg.transport), &cfg.queue_name)?; }
            }
        }
        Cmd::Doctor => doctor(&prices)?,
    }
    Ok(())
}

fn enrich(sd: &crate::model::SessionData) -> (GitStats, BeadsStats, Option<String>) {
    match sd.project.as_deref().map(Path::new) {
        Some(dir) if dir.exists() => {
            let g = git_stats(dir, sd.started_at, sd.ended_at).unwrap_or_default();
            let b = beads_stats(dir, sd.started_at, sd.ended_at);
            (g, b, current_branch(dir))
        }
        _ => (GitStats::default(), BeadsStats::default(), None),
    }
}

fn load_prices() -> PriceTable {
    let p = dirs::config_dir().unwrap_or_default().join("tokenprinter/prices.json");
    PriceTable::load(&p).unwrap_or_else(|_| PriceTable::embedded())
}

/// One receipt per agent that had activity on `date` (today if None).
fn daily_receipts(prices: &PriceTable, cfg: &Config, date: Option<&str>) -> anyhow::Result<Vec<crate::model::Receipt>> {
    use chrono::{NaiveDate, Utc};
    let day = match date {
        Some(d) => NaiveDate::parse_from_str(d, "%Y-%m-%d").context("date must be YYYY-MM-DD")?,
        None => Utc::now().date_naive(),
    };
    let mut out = Vec::new();
    for adapter in all_adapters() {
        let refs = adapter.discover()?;
        // merge all sessions for the day into one synthetic SessionData per agent
        let mut merged: Option<crate::model::SessionData> = None;
        for r in &refs {
            let sd = match adapter.parse(r) { Ok(s)=>s, Err(_)=>continue };
            if sd.records.iter().all(|rec| rec.timestamp.date_naive() != day) { continue; }
            match &mut merged {
                None => merged = Some(sd),
                Some(m) => {
                    m.records.extend(sd.records);
                    for (n,c) in sd.tool_calls {
                        if let Some(e)=m.tool_calls.iter_mut().find(|(x,_)| *x==n){ e.1+=c; }
                        else { m.tool_calls.push((n,c)); }
                    }
                    if sd.started_at < m.started_at { m.started_at = sd.started_at; }
                    if sd.ended_at > m.ended_at { m.ended_at = sd.ended_at; }
                    m.turns += sd.turns;
                }
            }
        }
        if let Some(mut m) = merged {
            m.session_id = format!("{} daily {}", adapter.agent().slug(), day);
            let mut rc = assemble_session(&m, prices, &cfg.location, GitStats::default(), BeadsStats::default());
            rc.scope = crate::model::Scope::Daily;
            out.push(rc);
        }
    }
    Ok(out)
}

fn doctor(prices: &PriceTable) -> anyhow::Result<()> {
    println!("tokenprinter doctor");
    println!("  config: {}", Config::path().display());
    // tool availability
    for (name, args) in [("lp", vec!["-v"]), ("git", vec!["--version"]), ("bd", vec!["--version"])] {
        let ok = std::process::Command::new(name).args(&args).output().map(|o| o.status.success()).unwrap_or(false);
        println!("  {:<5} {}", name, if ok {"ok"} else {"MISSING"});
    }
    // adapters + price drift
    for adapter in all_adapters() {
        let refs = adapter.discover().unwrap_or_default();
        println!("  {:<7} {} sessions", adapter.agent().slug(), refs.len());
        // drift check on up to 3 sessions that self-report cost
        let mut checked = 0;
        for r in refs.iter().take(50) {
            if checked >= 3 { break; }
            let sd = match adapter.parse(r) { Ok(s)=>s, Err(_)=>continue };
            for rec in &sd.records {
                if let Some(reported) = rec.cost {
                    let mut probe = rec.clone(); probe.cost = None;
                    if let Some(computed) = prices.cost_for(&probe) {
                        if reported > 0.0 {
                            let drift = ((computed - reported)/reported).abs()*100.0;
                            if drift > 1.0 {
                                println!("    DRIFT {} {}: reported ${:.4} computed ${:.4} ({:.1}%)",
                                    adapter.agent().slug(), rec.model, reported, computed, drift);
                            }
                            checked += 1;
                            if checked >= 3 { break; }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::SessionRef;
    use crate::model::Agent;
    use std::path::PathBuf;

    #[test]
    fn latest_session_picks_most_recent_by_mtime_is_skipped_use_first() {
        // helper picks by provided order's last element when mtimes equal/unknown
        let refs = vec![
            SessionRef{agent:Agent::Claude, session_id:"a".into(), path:PathBuf::from("tests/fixtures/claude_session.jsonl")},
        ];
        let pick = pick_session(&refs, None).unwrap();
        assert_eq!(pick.session_id, "a");
    }

    #[test]
    fn explicit_session_id_is_matched() {
        let refs = vec![
            SessionRef{agent:Agent::Claude, session_id:"x".into(), path:PathBuf::from("p1")},
            SessionRef{agent:Agent::Claude, session_id:"y".into(), path:PathBuf::from("p2")},
        ];
        let pick = pick_session(&refs, Some("y")).unwrap();
        assert_eq!(pick.session_id, "y");
    }
}
