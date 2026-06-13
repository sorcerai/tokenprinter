use crate::model::*;
use crate::pricing::PriceTable;
use std::collections::BTreeMap;

pub fn assemble_session(
    sd: &SessionData,
    prices: &PriceTable,
    location: &str,
    git: GitStats,
    beads: BeadsStats,
) -> Receipt {
    // per-model aggregation
    let mut by_model: BTreeMap<String, ModelLine> = BTreeMap::new();
    let mut total_cost = 0.0;
    let mut any_cost = false;
    let mut cache_saved_usd = 0.0;
    let mut total_tokens = 0u64;
    let mut cache_read_total = 0u64;
    let mut input_like_total = 0u64;

    for r in &sd.records {
        let line = by_model.entry(r.model.clone()).or_insert(ModelLine {
            model: r.model.clone(), input:0, output:0, cache_write:0, cache_read:0, cost:None,
        });
        line.input += r.input; line.output += r.output;
        line.cache_write += r.cache_write; line.cache_read += r.cache_read;
        if let Some(c) = prices.cost_for(r) {
            any_cost = true; total_cost += c;
            *line.cost.get_or_insert(0.0) += c;
        }
        if let Some(s) = prices.cache_savings(r) { cache_saved_usd += s; }
        total_tokens += r.total_tokens();
        cache_read_total += r.cache_read;
        input_like_total += r.input + r.cache_read;
    }

    // tools sorted by count desc
    let mut tools = sd.tool_calls.clone();
    tools.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    let duration_secs = (sd.ended_at - sd.started_at).num_seconds().max(0);
    let burn = if any_cost && duration_secs > 0 {
        Some(total_cost / (duration_secs as f64 / 3600.0))
    } else { None };

    let cache_hit_rate = if input_like_total > 0 {
        cache_read_total as f64 / input_like_total as f64 * 100.0
    } else { 0.0 };

    Receipt {
        scope: Scope::Session,
        agent: sd.agent,
        location: location.to_string(),
        session_name: sd.session_id.clone(),
        project: sd.project.clone(),
        git_branch: sd.git_branch.clone(),
        when: sd.ended_at,
        duration_secs,
        per_model: by_model.into_values().collect(),
        total_tokens,
        total_cost: if any_cost { Some(total_cost) } else { None },
        cache_saved_tokens: cache_read_total,
        cache_saved_usd: if any_cost { Some(cache_saved_usd) } else { None },
        cache_hit_rate,
        burn_rate_per_hr: burn,
        tools,
        git, beads,
        sparkline: sparkline(sd),
        precompact: false,
    }
}

/// 20-bucket token-volume sparkline over the session timeline (heights 0..=7).
/// Returns empty when there are fewer than 2 records (a single record produces a
/// meaningless one-full-bar result).
pub fn sparkline(sd: &SessionData) -> Vec<u8> {
    const N: usize = 20;
    if sd.records.len() < 2 { return vec![]; }
    let span = (sd.ended_at - sd.started_at).num_seconds().max(1) as f64;
    let mut buckets = [0u64; N];
    for r in &sd.records {
        let off = (r.timestamp - sd.started_at).num_seconds().max(0) as f64;
        let mut i = ((off / span) * N as f64) as usize;
        if i >= N { i = N - 1; }
        buckets[i] += r.total_tokens();
    }
    let max = *buckets.iter().max().unwrap_or(&0);
    if max == 0 { return vec![0; N]; }
    buckets.iter().map(|&b| ((b as f64 / max as f64) * 7.0).round() as u8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Agent, CacheTtl, SessionData, UsageRecord, GitStats, BeadsStats};
    use crate::pricing::PriceTable;
    use chrono::{Utc, Duration};

    fn rec(model:&str, input:u64, output:u64, cw:u64, cr:u64, cost:Option<f64>, off:i64) -> UsageRecord {
        let mut r = UsageRecord::zeroed(Agent::Claude, model);
        r.input=input; r.output=output; r.cache_write=cw; r.cache_read=cr;
        r.cost=cost; r.cache_write_ttl=CacheTtl::FiveMin;
        r.timestamp = Utc::now() + Duration::seconds(off); r
    }

    #[test]
    fn assembles_per_model_and_totals() {
        let start = Utc::now();
        let sd = SessionData {
            agent: Agent::Claude, session_id: "s".into(), project: Some("/tmp/x".into()),
            git_branch: Some("main".into()), started_at: start, ended_at: start + Duration::minutes(60),
            records: vec![
                rec("claude-opus-4-8", 1_000_000, 0, 0, 0, None, 0),   // $5
                rec("claude-opus-4-8", 0, 1_000_000, 0, 0, None, 30),  // $25
                rec("claude-sonnet-4-6", 1_000_000, 0, 0, 0, None, 60),// $3
            ],
            tool_calls: vec![("Edit".into(),3),("Bash".into(),1)], turns: 3,
        };
        let t = PriceTable::embedded();
        let r = assemble_session(&sd, &t, "Edinburgh, Scotland",
            GitStats{files_changed:2,added:10,removed:1,commits:1},
            BeadsStats{opened:vec!["tp-1".into()],closed:vec![]});
        assert_eq!(r.per_model.len(), 2);
        assert!((r.total_cost.unwrap() - 33.0).abs() < 1e-6, "{:?}", r.total_cost);
        assert_eq!(r.total_tokens, 3_000_000);
        assert!((r.burn_rate_per_hr.unwrap() - 33.0).abs() < 1e-3); // 60-min session
        assert_eq!(r.tools[0].0, "Edit"); // sorted desc by count
        assert_eq!(r.git.commits, 1);
        assert_eq!(r.beads.opened, vec!["tp-1"]);
    }
}
