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

/// Pure decision helper: given what the seen-map knows, should we print this session?
///
/// - If the key is absent from `seen`, we've never printed it → use `should_print`.
/// - If the key is present and the mtime is unchanged, skip (already handled or seeded).
/// - If the key is present but the mtime advanced, the session was modified after we last
///   saw it; apply `should_print` to decide whether it is now idle again.
///
/// This is extracted so it can be unit-tested without any I/O.
pub fn decide(seen: &HashMap<String, u64>, key: &str, mtime: u64, now: u64, idle: u64) -> bool {
    match seen.get(key) {
        // Already in seen-map at this exact mtime → either seeded or already printed.
        Some(&recorded_mtime) if recorded_mtime == mtime => false,
        // Key absent, or mtime advanced since last record → check idle threshold.
        _ => should_print(now, mtime, idle, false),
    }
}

/// Build a seen-map key for a session file.
///
/// We use the **absolute path** as the discriminator rather than session_id because
/// some adapters (e.g. Pi subagent artifacts) can produce multiple files that share
/// the same session_id string (e.g. `"session"`).  Paths are always unique.
fn seen_key(sref: &crate::adapters::SessionRef) -> String {
    sref.path.display().to_string()
}

/// Populate `seen` with every currently-discoverable session for `agent`, keyed by
/// file path and valued at the file's current mtime.
///
/// This seeds the map before the first printing pass so that pre-existing (historical)
/// sessions are never printed.  Only sessions that are written *after* this point —
/// and then go quiet — will be eligible for printing.
fn seed_seen(agent: Agent, seen: &mut HashMap<String, u64>) {
    let adapter = match adapter_for(agent) {
        Some(a) => a,
        None => return,
    };
    let refs = match adapter.discover() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("tokenprinter watch: seed discover {} error: {e}", agent.slug());
            return;
        }
    };
    for sref in &refs {
        let mtime_secs = match std::fs::metadata(&sref.path).and_then(|m| m.modified()) {
            Ok(t) => t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_secs(),
            Err(_) => continue,
        };
        seen.insert(seen_key(&sref), mtime_secs);
    }
}

/// Watch codex and pi session directories; print receipts for sessions that have
/// gone idle (no writes for `cfg.idle_seconds`).
///
/// `once = true` performs a single pass and returns. Otherwise the loop runs
/// indefinitely with a 10-second sleep between passes.
///
/// Claude sessions are intentionally excluded — they are handled via hooks (Task 2).
///
/// **Note**: the watcher only prints sessions that go idle during its uptime;
/// pre-existing sessions present at startup are intentionally ignored.
pub fn watch_loop(once: bool, preview: bool, cfg: &Config, prices: &PriceTable) -> anyhow::Result<()> {
    // seen: "agent:session_id" -> mtime at which we last printed (or seeded mtime)
    let mut seen: HashMap<String, u64> = HashMap::new();

    // Seed the seen-map with all currently-discoverable sessions so we never
    // print the historical backlog that existed before this daemon started.
    for agent in [Agent::Codex, Agent::Pi] {
        seed_seen(agent, &mut seen);
    }

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

                let key = seen_key(sref);

                if decide(&seen, &key, mtime_secs, now_secs, cfg.idle_seconds) {
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
    use super::{decide, should_print};
    use std::collections::HashMap;

    // ── should_print tests (unchanged) ────────────────────────────────────────

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

    // ── decide / seeding tests ─────────────────────────────────────────────────

    /// A session seeded into seen with its current mtime must NOT be printed on a
    /// subsequent idle pass where the mtime is unchanged.
    #[test]
    fn seeded_session_unchanged_mtime_is_not_printed() {
        let mut seen = HashMap::new();
        let key = "codex:abc123";
        let mtime: u64 = 1_000_000;
        // Simulate seed: insert current mtime.
        seen.insert(key.to_string(), mtime);

        // On the next pass: mtime still the same, session is old → decide must be false.
        let now = mtime + 200; // well past idle threshold of 90s
        assert!(
            !decide(&seen, key, mtime, now, 90),
            "seeded session with unchanged mtime should NOT be printed"
        );
    }

    /// A session seeded at startup (mtime_seed) that gets new writes (mtime_new)
    /// and then goes idle should be printed on the next pass.
    #[test]
    fn session_modified_after_seed_prints_when_idle() {
        let mut seen = HashMap::new();
        let key = "codex:abc123";
        let mtime_seed: u64 = 1_000_000;
        let mtime_new: u64 = 1_001_000; // written to after startup

        // Seed pass: record original mtime.
        seen.insert(key.to_string(), mtime_seed);

        // Pass 2: mtime advanced but not yet idle.
        let now_active = mtime_new + 30; // 30s after last write, idle threshold 90s
        assert!(
            !decide(&seen, key, mtime_new, now_active, 90),
            "recently modified session should NOT print before idle threshold"
        );

        // Pass 3: now idle.
        let now_idle = mtime_new + 100; // 100s after last write > 90s threshold
        assert!(
            decide(&seen, key, mtime_new, now_idle, 90),
            "modified-then-idle session SHOULD be printed"
        );
    }

    /// A brand-new session (never seen before) that is idle should be printed.
    #[test]
    fn new_unseen_idle_session_is_printed() {
        let seen: HashMap<String, u64> = HashMap::new();
        let mtime: u64 = 1_000_000;
        let now = mtime + 200;
        assert!(
            decide(&seen, "pi:newsession", mtime, now, 90),
            "new session not in seen-map should be printed when idle"
        );
    }

    /// A brand-new session (never seen before) that is NOT yet idle should not print.
    #[test]
    fn new_unseen_active_session_is_not_printed() {
        let seen: HashMap<String, u64> = HashMap::new();
        let mtime: u64 = 1_000_000;
        let now = mtime + 30;
        assert!(
            !decide(&seen, "pi:newsession", mtime, now, 90),
            "new session that is not yet idle should NOT print"
        );
    }

    /// Two-pass simulation: pass 1 seeds (no prints), mtime advances, pass 2 prints.
    #[test]
    fn two_pass_seed_then_modified_then_idle() {
        let mut seen: HashMap<String, u64> = HashMap::new();
        let key = "codex:twopass";
        let mtime_at_seed: u64 = 5_000;
        let idle_secs: u64 = 90;

        // --- PASS 1: seed phase ---
        // Simulate what seed_seen does: insert current mtime, do NOT print.
        seen.insert(key.to_string(), mtime_at_seed);
        // Confirm: even though this session would be "idle" by age, decide returns false.
        let now_pass1 = mtime_at_seed + 500;
        assert!(
            !decide(&seen, key, mtime_at_seed, now_pass1, idle_secs),
            "pass 1 (seeded): must not print"
        );

        // --- Session gets new activity after daemon started ---
        let mtime_active = mtime_at_seed + 600; // file written to after seed

        // --- PASS 2a: session still active (< idle_secs since last write) ---
        let now_active = mtime_active + 10;
        assert!(
            !decide(&seen, key, mtime_active, now_active, idle_secs),
            "pass 2a: session still active, must not print"
        );

        // --- PASS 2b: session now idle ---
        let now_idle = mtime_active + 100; // 100s > 90s idle threshold
        assert!(
            decide(&seen, key, mtime_active, now_idle, idle_secs),
            "pass 2b: session idle after modification, must print"
        );

        // Simulate marking as printed.
        seen.insert(key.to_string(), mtime_active);

        // --- PASS 3: same mtime, already printed → no double-print ---
        assert!(
            !decide(&seen, key, mtime_active, now_idle + 100, idle_secs),
            "pass 3: already printed at this mtime, must not re-print"
        );
    }
}
