use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::adapters::adapter_for;
use crate::cli::print_session;
use crate::config::Config;
use crate::model::Agent;
use crate::pricing::PriceTable;

/// Pure, clock/IO-free idle decision.
///
/// Returns `true` when the session file is idle and has not yet been printed:
/// - `now_secs - mtime_secs >= idle_secs` (saturating, so clock-skew/future mtimes are safe)
/// - `already_printed` is false
pub fn should_print(now_secs: u64, mtime_secs: u64, idle_secs: u64, already_printed: bool) -> bool {
    !already_printed && now_secs.saturating_sub(mtime_secs) >= idle_secs
}

/// Watch codex and pi session directories; print receipts for sessions that have
/// gone idle (no writes for `cfg.idle_seconds`).
///
/// `once = true` performs a single pass and returns. Otherwise the loop runs
/// indefinitely with a 10-second sleep between passes.
///
/// Claude sessions are intentionally excluded — they are handled via hooks (Task 2).
pub fn watch_loop(once: bool, preview: bool, cfg: &Config, prices: &PriceTable) -> anyhow::Result<()> {
    // seen: "agent:session_id" -> mtime at which we last printed
    let mut seen: HashMap<String, u64> = HashMap::new();

    loop {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();

        for agent in [Agent::Codex, Agent::Pi] {
            let adapter = match adapter_for(agent) {
                Some(a) => a,
                None => continue,
            };

            let refs = match adapter.discover() {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("tokenprinter watch: discover {} error: {e}", agent.slug());
                    continue;
                }
            };

            for sref in &refs {
                let mtime_secs = match std::fs::metadata(&sref.path)
                    .and_then(|m| m.modified())
                {
                    Ok(t) => t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_secs(),
                    Err(e) => {
                        eprintln!(
                            "tokenprinter watch: stat {} error: {e}",
                            sref.path.display()
                        );
                        continue;
                    }
                };

                let key = format!("{}:{}", agent.slug(), sref.session_id);
                let already_printed = seen.get(&key) == Some(&mtime_secs);

                if should_print(now_secs, mtime_secs, cfg.idle_seconds, already_printed) {
                    match print_session(adapter.as_ref(), sref, cfg, prices, preview) {
                        Ok(()) => {
                            seen.insert(key, mtime_secs);
                            if !preview {
                                eprintln!(
                                    "tokenprinter watch: printed {} receipt for {}",
                                    agent.slug(),
                                    sref.session_id
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "tokenprinter watch: parse/print {} {} error: {e}",
                                agent.slug(),
                                sref.session_id
                            );
                        }
                    }
                }
            }
        }

        if once {
            break;
        }
        std::thread::sleep(Duration::from_secs(10));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::should_print;

    /// Idle elapsed and not yet printed → should print.
    #[test]
    fn idle_elapsed_not_printed_returns_true() {
        assert!(should_print(1000, 800, 90, false));
    }

    /// File modified only 30s ago, idle threshold is 90s → not yet idle.
    #[test]
    fn not_idle_yet_returns_false() {
        assert!(!should_print(1000, 970, 90, false));
    }

    /// Already printed at this mtime → do not print again.
    #[test]
    fn already_printed_returns_false() {
        assert!(!should_print(1000, 800, 90, true));
    }

    /// mtime is in the future (clock skew): saturating_sub yields 0 < idle_secs → false.
    #[test]
    fn future_mtime_clock_skew_returns_false() {
        assert!(!should_print(1000, 2000, 90, false));
    }

    /// Exactly at idle boundary (elapsed == idle_secs) → print.
    #[test]
    fn exactly_at_idle_boundary_returns_true() {
        assert!(should_print(1090, 1000, 90, false));
    }

    /// One second short of idle → do not print.
    #[test]
    fn one_second_short_of_idle_returns_false() {
        assert!(!should_print(1089, 1000, 90, false));
    }
}
