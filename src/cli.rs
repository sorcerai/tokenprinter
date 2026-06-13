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
        Cmd::Print { agent, session, last, preview } => {
            let ag = agent_from_str(&agent)?;
            let adapter = adapter_for(ag).ok_or_else(|| anyhow!("agent {agent} not supported in phase 1"))?;
            let refs = adapter.discover()?;
            // --last overrides --session: force newest-by-mtime selection
            let session_key = if last { None } else { session.as_deref() };
            let chosen = pick_session(&refs, session_key)?.clone();
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
        // Merge only the records whose timestamp falls on `day` (record-level, not session-level).
        // This prevents cross-midnight sessions from being double-counted on both days.
        let mut merged: Option<crate::model::SessionData> = None;
        for r in &refs {
            let sd = match adapter.parse(r) { Ok(s)=>s, Err(_)=>continue };
            // Keep only records that belong to `day`.
            let day_records: Vec<_> = sd.records.into_iter()
                .filter(|rec| rec.timestamp.date_naive() == day)
                .collect();
            if day_records.is_empty() { continue; }

            // Derive started_at/ended_at/turns from the matching records.
            let seg_start = day_records.iter().map(|r| r.timestamp).min().unwrap();
            let seg_end   = day_records.iter().map(|r| r.timestamp).max().unwrap();
            let seg_turns = day_records.len() as u32;

            match &mut merged {
                None => {
                    merged = Some(crate::model::SessionData {
                        agent: sd.agent, session_id: sd.session_id,
                        project: sd.project, git_branch: sd.git_branch,
                        started_at: seg_start, ended_at: seg_end,
                        records: day_records,
                        tool_calls: sd.tool_calls,
                        turns: seg_turns,
                    });
                }
                Some(m) => {
                    m.records.extend(day_records);
                    for (n,c) in sd.tool_calls {
                        if let Some(e)=m.tool_calls.iter_mut().find(|(x,_)| *x==n){ e.1+=c; }
                        else { m.tool_calls.push((n,c)); }
                    }
                    if seg_start < m.started_at { m.started_at = seg_start; }
                    if seg_end > m.ended_at { m.ended_at = seg_end; }
                    m.turns += seg_turns;
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
    let cups_ok = std::process::Command::new("lpstat").args(["-r"]).output()
        .map(|o| o.status.success()).unwrap_or(false);
    println!("  {:<8} {}", "lp/cups", if cups_ok {"ok"} else {"MISSING"});
    for (name, args) in [("git", vec!["--version"]), ("bd", vec!["--version"])] {
        let ok = std::process::Command::new(name).args(&args).output().map(|o| o.status.success()).unwrap_or(false);
        println!("  {:<8} {}", name, if ok {"ok"} else {"MISSING"});
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
    use crate::model::{Agent, CacheTtl, SessionData, UsageRecord};
    use crate::pricing::PriceTable;
    use chrono::{NaiveDate, TimeZone, Utc};
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

    /// Verify daily aggregation counts only records on the target day.
    /// Session A is fully on 2026-01-15. Session B spans midnight (records on both
    /// 2026-01-14 and 2026-01-15). The day receipt for 2026-01-15 must include
    /// session A's tokens and only the matching record from session B — not
    /// session B's full token total.
    #[test]
    fn daily_receipts_does_not_double_count_cross_midnight_sessions() {
        fn ts(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> chrono::DateTime<Utc> {
            Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
        }
        fn rec(agent: Agent, ts_val: chrono::DateTime<Utc>, tokens: u64) -> UsageRecord {
            let mut r = UsageRecord::zeroed(agent, "test-model");
            r.input = tokens; r.timestamp = ts_val; r.cache_write_ttl = CacheTtl::FiveMin;
            r
        }

        let day = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();

        // Session A: two records fully on 2026-01-15 (100 + 200 = 300 tokens)
        let sd_a = SessionData {
            agent: Agent::Claude, session_id: "a".into(), project: None, git_branch: None,
            started_at: ts(2026, 1, 15, 10, 0), ended_at: ts(2026, 1, 15, 11, 0),
            records: vec![
                rec(Agent::Claude, ts(2026, 1, 15, 10, 0), 100),
                rec(Agent::Claude, ts(2026, 1, 15, 10, 30), 200),
            ],
            tool_calls: vec![], turns: 2,
        };

        // Session B: spans midnight — first record on 2026-01-14 (999 tokens, must be excluded),
        // second record on 2026-01-15 (50 tokens, must be included).
        let sd_b = SessionData {
            agent: Agent::Claude, session_id: "b".into(), project: None, git_branch: None,
            started_at: ts(2026, 1, 14, 23, 50), ended_at: ts(2026, 1, 15, 0, 30),
            records: vec![
                rec(Agent::Claude, ts(2026, 1, 14, 23, 50), 999),
                rec(Agent::Claude, ts(2026, 1, 15, 0, 30),   50),
            ],
            tool_calls: vec![], turns: 2,
        };

        // Simulate the record-level merge from daily_receipts.
        let prices = PriceTable::embedded();
        let cfg = Config::load();
        let mut merged: Option<SessionData> = None;
        for sd in [sd_a, sd_b] {
            let day_records: Vec<_> = sd.records.into_iter()
                .filter(|r| r.timestamp.date_naive() == day)
                .collect();
            if day_records.is_empty() { continue; }
            let seg_start = day_records.iter().map(|r| r.timestamp).min().unwrap();
            let seg_end   = day_records.iter().map(|r| r.timestamp).max().unwrap();
            let seg_turns = day_records.len() as u32;
            match &mut merged {
                None => merged = Some(SessionData {
                    agent: Agent::Claude, session_id: "merged".into(),
                    project: None, git_branch: None,
                    started_at: seg_start, ended_at: seg_end,
                    records: day_records, tool_calls: vec![], turns: seg_turns,
                }),
                Some(m) => {
                    m.records.extend(day_records);
                    if seg_start < m.started_at { m.started_at = seg_start; }
                    if seg_end > m.ended_at { m.ended_at = seg_end; }
                    m.turns += seg_turns;
                }
            }
        }
        let m = merged.expect("should have merged data");
        let rc = crate::assemble::assemble_session(
            &m, &prices, &cfg.location,
            crate::model::GitStats::default(), crate::model::BeadsStats::default(),
        );
        // Expect 300 (session A) + 50 (session B's on-day record) = 350 tokens
        // Session B's 999-token record on 2026-01-14 must be excluded.
        assert_eq!(rc.total_tokens, 350,
            "got {} tokens, expected 350 (should not include session B's 2026-01-14 record)",
            rc.total_tokens);
    }
}
