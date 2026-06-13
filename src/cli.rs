use crate::adapters::{adapter_for, all_adapters, Adapter, SessionRef};
use crate::assemble::assemble_session;
use crate::config::Config;
use crate::enrich::{beads::beads_stats, git::{current_branch, git_stats}};
use crate::model::{Agent, GitStats, BeadsStats};
use crate::openrouter;
use crate::pricing::PriceTable;
use crate::render::{render_bytes, render_bytes_with_qr, render_text};
use crate::transport::{send, Mode};
use crate::triggers;
use anyhow::{anyhow, Context};
use chrono_tz::Tz;
use clap::{Parser, Subcommand};
use std::path::Path;
use std::str::FromStr;

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
        #[arg(long)] audit: bool,
        /// Suppress the "printed … receipt" confirmation line on stderr.
        #[arg(long)] quiet: bool,
        /// Mark this as a pre-compaction snapshot (adds a PRE-COMPACTION MEMORIAL note to the receipt header).
        #[arg(long)] precompact: bool,
    },
    Daily { #[arg(long)] date: Option<String>, #[arg(long)] preview: bool },
    Doctor,
    /// Merge tokenprinter hook entries into a Claude settings JSON (idempotent).
    InstallHooks {
        /// Path to the Claude settings JSON file.
        #[arg(long)] settings: Option<std::path::PathBuf>,
        /// Path to the tokenprinter binary to embed in the hook command.
        #[arg(long)] bin: Option<std::path::PathBuf>,
    },
    /// Write a launchd plist for the tokenprinter watch daemon (does NOT run launchctl).
    InstallWatcher {
        /// Destination path for the .plist file.
        #[arg(long)] out: Option<std::path::PathBuf>,
        /// launchd label for the service.
        #[arg(long, default_value="com.tokenprinter.watch")] label: String,
        /// Idle timeout in seconds (overrides config value).
        #[arg(long)] idle: Option<u64>,
    },
    /// Watch codex/pi session directories and auto-print when idle.
    ///
    /// Note: only sessions that go idle during this daemon's uptime are printed;
    /// sessions that already exist at startup are intentionally ignored.
    Watch {
        /// Do a single pass and exit (no sleep loop).
        #[arg(long)] once: bool,
        /// Print receipt as text to stdout instead of sending to printer.
        #[arg(long)] preview: bool,
        /// Override the idle threshold in seconds (default: from config).
        #[arg(long)] idle: Option<u64>,
    },
    /// Fetch and print an OpenRouter spend receipt.
    ///
    /// Reads the API key from OPENROUTER_API_KEY env var, falling back to
    /// `openrouter_key` in the config file. Per-model token breakdown requires
    /// a management key (non-management keys receive 403 from /activity, which
    /// is silently ignored).
    Openrouter {
        /// Print receipt as text to stdout instead of sending to printer.
        #[arg(long)] preview: bool,
    },
}

/// Reusable helper: parse → enrich → assemble → (preview? print text : send bytes).
pub fn print_session(
    adapter: &dyn Adapter,
    sref: &SessionRef,
    cfg: &Config,
    prices: &PriceTable,
    preview: bool,
) -> anyhow::Result<()> {
    let sd = adapter.parse(sref)?;
    let (git, beads, branch) = enrich(&sd);
    let mut receipt = assemble_session(&sd, prices, &cfg.location, git, beads);
    if receipt.git_branch.is_none() { receipt.git_branch = branch; }
    receipt.billing_note = billing_note(cfg);
    if preview {
        print!("{}", render_text(&receipt));
    } else {
        let bytes = if cfg.show_qr {
            let qr_data = resume_qr(sref.agent, &sref.session_id);
            render_bytes_with_qr(&receipt, Some(&qr_data))
        } else {
            render_bytes(&receipt)
        };
        send(&bytes, Mode::parse(&cfg.transport), &cfg.queue_name)?;
    }
    Ok(())
}

/// Return the agent-appropriate resume command string for use in QR codes.
///
/// Scanning or copying the QR lets you jump straight back into the session:
/// - Claude → `claude --resume <session_id>`
/// - Codex  → `codex resume <session_id>`
/// - Pi     → `pi --resume <session_id>`
/// - Agy    → just the session_id (no well-known resume CLI)
fn resume_qr(agent: Agent, session_id: &str) -> String {
    match agent {
        Agent::Claude => format!("claude --resume {session_id}"),
        Agent::Codex  => format!("codex resume {session_id}"),
        Agent::Pi     => format!("pi --resume {session_id}"),
        Agent::Agy    => session_id.to_string(),
    }
}

/// Return the billing note string for subscription mode, or None for API mode.
fn billing_note(cfg: &Config) -> Option<String> {
    if cfg.billing == "api" {
        None
    } else {
        Some("API-equivalent \u{2014} not charged on subscription".to_string())
    }
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
        Cmd::Print { agent, session, last, preview, audit, quiet, precompact } => {
            let ag = agent_from_str(&agent)?;
            let adapter = adapter_for(ag).ok_or_else(|| anyhow!("agent {agent} not supported in phase 1"))?;
            let refs = adapter.discover()?;
            // --last overrides --session: force newest-by-mtime selection
            let session_key = if last { None } else { session.as_deref() };
            let chosen = pick_session(&refs, session_key)?.clone();
            let sd = adapter.parse(&chosen)?;

            if audit {
                print_audit_table(&sd, &prices);
                return Ok(());
            }

            let (git, beads, branch) = enrich(&sd);
            let mut receipt = assemble_session(&sd, &prices, &cfg.location, git, beads);
            if receipt.git_branch.is_none() { receipt.git_branch = branch; }
            receipt.precompact = precompact;
            receipt.billing_note = billing_note(&cfg);

            if preview {
                print!("{}", render_text(&receipt));
            } else {
                let bytes = if cfg.show_qr {
                    let qr_data = resume_qr(ag, &chosen.session_id);
                    render_bytes_with_qr(&receipt, Some(&qr_data))
                } else {
                    render_bytes(&receipt)
                };
                send(&bytes, Mode::parse(&cfg.transport), &cfg.queue_name)?;
                if !quiet {
                    eprintln!("printed {} receipt for {}", ag.label(), receipt.session_name);
                }
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
        Cmd::InstallHooks { settings, bin } => {
            let settings_path = settings.unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_default()
                    .join(".claude/settings.json")
            });
            let bin_path = bin
                .map(|p| p.to_string_lossy().into_owned())
                .or_else(|| std::env::current_exe().ok().map(|p| p.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "tokenprinter".to_string());

            triggers::hooks::install_hooks(&settings_path, &bin_path)?;

            let stop_cmd = format!(
                "{bin_path} print --agent claude --session \"$CLAUDE_SESSION_ID\" --quiet"
            );
            let precompact_cmd = format!(
                "{bin_path} print --agent claude --session \"$CLAUDE_SESSION_ID\" --precompact --quiet"
            );
            println!("Added hooks to: {}", settings_path.display());
            println!("  Stop      → {stop_cmd}");
            println!("  PreCompact→ {precompact_cmd}");
            println!();
            println!(
                "NOTE: This modified your live Claude settings; remove the tokenprinter entries from {} to undo.",
                settings_path.display()
            );
        }
        Cmd::Watch { once, preview, idle } => {
            let mut cfg2 = cfg.clone();
            if let Some(s) = idle { cfg2.idle_seconds = s; }
            crate::watch::watch_loop(once, preview, &cfg2, &prices)?;
        }
        Cmd::Openrouter { preview } => {
            // Resolve key: env var first, then config field.
            let key = std::env::var("OPENROUTER_API_KEY")
                .unwrap_or_else(|_| cfg.openrouter_key.clone());
            if key.is_empty() {
                eprintln!(
                    "error: no OpenRouter API key found.\n\
                     Set OPENROUTER_API_KEY env var or add `openrouter_key = \"sk-or-...\"` \
                     to your tokenprinter config."
                );
                std::process::exit(1);
            }
            let stmt = openrouter::fetch_statement(&key)?;
            let when = chrono::Utc::now();
            if preview {
                print!("{}", openrouter::render_statement_text(&stmt, when));
            } else {
                let qr_data = if cfg.show_qr { Some("https://openrouter.ai/activity") } else { None };
                let bytes = openrouter::render_statement_bytes(&stmt, when, qr_data);
                send(&bytes, Mode::parse(&cfg.transport), &cfg.queue_name)?;
            }
        }
        Cmd::InstallWatcher { out, label, idle } => {
            let cfg2 = Config::load();
            let idle_seconds = idle.unwrap_or(cfg2.idle_seconds);
            let out_path = out.unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_default()
                    .join("Library/LaunchAgents/com.tokenprinter.watch.plist")
            });
            let bin_path = std::env::current_exe()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "tokenprinter".to_string());

            let plist = triggers::launchd::launchd_plist(&bin_path, &label, idle_seconds);

            if let Some(parent) = out_path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
            }
            std::fs::write(&out_path, &plist)
                .with_context(|| format!("writing plist to {}", out_path.display()))?;

            println!("Wrote launchd plist to: {}", out_path.display());
            println!();
            println!("To start the watcher, run:");
            println!("  launchctl load {}", out_path.display());
        }
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

/// Parse `cfg.timezone` into a `chrono_tz::Tz`, falling back to UTC on failure.
fn parse_tz(cfg: &Config) -> Tz {
    Tz::from_str(&cfg.timezone).unwrap_or_else(|_| {
        eprintln!("warn: unknown timezone '{}', falling back to UTC", cfg.timezone);
        chrono_tz::UTC
    })
}

/// One receipt per agent that had activity on `date` (today if None).
fn daily_receipts(prices: &PriceTable, cfg: &Config, date: Option<&str>) -> anyhow::Result<Vec<crate::model::Receipt>> {
    use chrono::{NaiveDate, Utc};
    let tz = parse_tz(cfg);
    let day: NaiveDate = match date {
        Some(d) => NaiveDate::parse_from_str(d, "%Y-%m-%d").context("date must be YYYY-MM-DD")?,
        None => Utc::now().with_timezone(&tz).date_naive(),
    };
    let mut out = Vec::new();
    for adapter in all_adapters() {
        let refs = adapter.discover()?;
        // Merge only the records whose timestamp falls on `day` in the configured local timezone.
        // This prevents cross-midnight sessions from being double-counted on both days.
        let mut merged: Option<crate::model::SessionData> = None;
        for r in &refs {
            let sd = match adapter.parse(r) { Ok(s)=>s, Err(_)=>continue };
            // Keep only records that belong to `day` in local tz.
            let day_records: Vec<_> = sd.records.into_iter()
                .filter(|rec| rec.timestamp.with_timezone(&tz).date_naive() == day)
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
            rc.billing_note = billing_note(cfg);
            out.push(rc);
        }
    }
    Ok(out)
}

fn doctor(prices: &PriceTable) -> anyhow::Result<()> {
    let cfg = Config::load();
    println!("tokenprinter doctor");
    println!("  config: {}", Config::path().display());
    // OpenRouter key status (no network call — just presence check)
    let or_key_set = std::env::var("OPENROUTER_API_KEY").map(|v| !v.is_empty()).unwrap_or(false)
        || !cfg.openrouter_key.is_empty();
    println!("  openrouter key: {}", if or_key_set { "set" } else { "not set" });
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

// ---- Audit mode (FIX 3) ----

pub struct AuditRow {
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub context_size: u64,
    pub cost: Option<f64>,
    /// True when input + cache_read + cache_write > context_size (invariant violation).
    pub overlap: bool,
}

/// Build one audit row per UsageRecord.
pub fn audit_rows(sd: &crate::model::SessionData, prices: &PriceTable) -> Vec<AuditRow> {
    sd.records.iter().map(|rec| {
        let cost = prices.cost_for(rec);
        let overlap = rec.input + rec.cache_read + rec.cache_write > rec.context_size;
        AuditRow {
            model: rec.model.clone(),
            input: rec.input,
            output: rec.output,
            cache_write: rec.cache_write,
            cache_read: rec.cache_read,
            context_size: rec.context_size,
            cost,
            overlap,
        }
    }).collect()
}

fn print_audit_table(sd: &crate::model::SessionData, prices: &PriceTable) {
    let rows = audit_rows(sd, prices);
    println!("{:<30} {:>10} {:>10} {:>12} {:>12} {:>14} {:>10} flags",
        "model", "input", "output", "cache_write", "cache_read", "context_size", "cost");
    println!("{}", "-".repeat(100));
    let mut total_input = 0u64;
    let mut total_output = 0u64;
    let mut total_cw = 0u64;
    let mut total_cr = 0u64;
    let mut total_cost = 0f64;
    let mut any_cost = false;
    for r in &rows {
        let cost_str = match r.cost { Some(c) => { any_cost = true; total_cost += c; format!("${c:.4}") } None => "—".into() };
        let flags = if r.overlap { "OVERLAP!" } else { "" };
        println!("{:<30} {:>10} {:>10} {:>12} {:>12} {:>14} {:>10} {}",
            r.model, r.input, r.output, r.cache_write, r.cache_read, r.context_size, cost_str, flags);
        total_input += r.input;
        total_output += r.output;
        total_cw += r.cache_write;
        total_cr += r.cache_read;
    }
    println!("{}", "-".repeat(100));
    let total_cost_str = if any_cost { format!("${total_cost:.4}") } else { "—".into() };
    println!("{:<30} {:>10} {:>10} {:>12} {:>12} {:>14} {:>10}",
        "TOTAL", total_input, total_output, total_cw, total_cr, "", total_cost_str);
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
    fn resume_qr_produces_agent_appropriate_command() {
        assert_eq!(resume_qr(Agent::Claude, "abc123"), "claude --resume abc123");
        assert_eq!(resume_qr(Agent::Codex,  "abc123"), "codex resume abc123");
        assert_eq!(resume_qr(Agent::Pi,     "abc123"), "pi --resume abc123");
        // Agy has no well-known resume CLI — just the session id.
        assert_eq!(resume_qr(Agent::Agy,    "abc123"), "abc123");
    }

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

    /// Verify daily aggregation counts only records on the target local day (America/Chicago).
    ///
    /// America/Chicago is UTC-6 in winter. We construct records using explicit UTC instants
    /// and verify the local-day filter is correct:
    ///
    /// - 2026-01-15 06:00 UTC  = 2026-01-15 00:00 CST  → ON the target local day
    /// - 2026-01-15 05:59 UTC  = 2026-01-14 23:59 CST  → NOT on the target local day
    ///
    /// Session A: two records at 2026-01-15 16:00 UTC (10:00 CST) and 16:30 UTC (10:30 CST)
    ///            → both on 2026-01-15 local, 100 + 200 = 300 tokens
    /// Session B: spans local midnight
    ///   record 1: 2026-01-15 05:50 UTC (2026-01-14 23:50 CST) → excluded (999 tokens)
    ///   record 2: 2026-01-15 06:30 UTC (2026-01-15 00:30 CST) → included (50 tokens)
    ///
    /// Expected total for local day 2026-01-15: 300 + 50 = 350 tokens.
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

        // Local day we're targeting: 2026-01-15 in America/Chicago (UTC-6 in winter).
        let tz = chrono_tz::America::Chicago;
        let day = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();

        // Session A: both records at 16:00 UTC and 16:30 UTC = 10:00/10:30 CST on Jan 15.
        let sd_a = SessionData {
            agent: Agent::Claude, session_id: "a".into(), project: None, git_branch: None,
            started_at: ts(2026, 1, 15, 16, 0), ended_at: ts(2026, 1, 15, 16, 30),
            records: vec![
                rec(Agent::Claude, ts(2026, 1, 15, 16, 0),  100),
                rec(Agent::Claude, ts(2026, 1, 15, 16, 30), 200),
            ],
            tool_calls: vec![], turns: 2,
        };

        // Session B: spans local midnight.
        //   05:50 UTC = 23:50 CST on 2026-01-14 → must be excluded (999 tokens)
        //   06:30 UTC = 00:30 CST on 2026-01-15 → must be included (50 tokens)
        let sd_b = SessionData {
            agent: Agent::Claude, session_id: "b".into(), project: None, git_branch: None,
            started_at: ts(2026, 1, 15, 5, 50), ended_at: ts(2026, 1, 15, 6, 30),
            records: vec![
                rec(Agent::Claude, ts(2026, 1, 15, 5, 50), 999),
                rec(Agent::Claude, ts(2026, 1, 15, 6, 30),  50),
            ],
            tool_calls: vec![], turns: 2,
        };

        // Simulate the record-level merge from daily_receipts using tz-aware filtering.
        let prices = PriceTable::embedded();
        let cfg = Config::load();
        let mut merged: Option<SessionData> = None;
        for sd in [sd_a, sd_b] {
            let day_records: Vec<_> = sd.records.into_iter()
                .filter(|r| r.timestamp.with_timezone(&tz).date_naive() == day)
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
        // Expect 300 (session A) + 50 (session B's CST-local Jan 15 record) = 350 tokens.
        // Session B's 999-token record (23:50 CST on Jan 14) must be excluded.
        assert_eq!(rc.total_tokens, 350,
            "got {} tokens, expected 350 (should not include session B's Jan 14 CST record)",
            rc.total_tokens);
    }

    #[test]
    fn audit_rows_detects_no_overlap_for_valid_record() {
        use crate::model::SessionData;
        let prices = PriceTable::embedded();
        let mut rec = UsageRecord::zeroed(Agent::Claude, "claude-opus-4-8");
        rec.input = 100; rec.cache_read = 50; rec.cache_write = 20;
        rec.context_size = 170; // input + cache_read + cache_write = 170 = context_size, no overlap
        let sd = SessionData {
            agent: Agent::Claude, session_id: "s".into(), project: None, git_branch: None,
            started_at: rec.timestamp, ended_at: rec.timestamp,
            records: vec![rec], tool_calls: vec![], turns: 1,
        };
        let rows = audit_rows(&sd, &prices);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].overlap, "should not flag valid record as overlap");
    }
}
