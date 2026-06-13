use crate::model::BeadsStats;
use crate::proc::output_with_timeout;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::process::Command;

fn ts(v: &Value, keys: &[&str]) -> Option<DateTime<Utc>> {
    for k in keys {
        if let Some(s) = v.get(*k).and_then(|x| x.as_str()) {
            if let Ok(d) = DateTime::parse_from_rfc3339(s) { return Some(d.with_timezone(&Utc)); }
        }
    }
    None
}

/// Pure: partition a `bd list --json` array into opened/closed within [start,end].
pub fn partition_beads(json: &str, start: DateTime<Utc>, end: DateTime<Utc>) -> anyhow::Result<BeadsStats> {
    let arr: Vec<Value> = serde_json::from_str(json)?;
    let mut b = BeadsStats::default();
    for issue in arr {
        let id = issue.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
        if id.is_empty() { continue; }
        if let Some(c) = ts(&issue, &["created","created_at"]) {
            if c >= start && c <= end { b.opened.push(id.clone()); }
        }
        if let Some(cl) = ts(&issue, &["closed","closed_at"]) {
            if cl >= start && cl <= end { b.closed.push(id.clone()); }
        }
    }
    Ok(b)
}

/// Thin wrapper: run `bd list --json` in `dir` (if `bd` exists) and partition.
pub fn beads_stats(dir: &std::path::Path, start: DateTime<Utc>, end: DateTime<Utc>) -> BeadsStats {
    let mut cmd = Command::new("bd");
    cmd.current_dir(dir).args(["list","--json"]);
    match output_with_timeout(cmd, 10) {
        Ok(o) if o.status.success() => {
            partition_beads(&String::from_utf8_lossy(&o.stdout), start, end).unwrap_or_default()
        }
        _ => BeadsStats::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Utc, Duration};

    #[test]
    fn partitions_opened_and_closed_in_window() {
        let now = Utc::now();
        let start = now - Duration::minutes(10);
        let end = now + Duration::minutes(10);
        let json = format!(r#"[
          {{"id":"tp-1","created":"{c}","status":"open"}},
          {{"id":"tp-2","created":"2000-01-01T00:00:00Z","closed":"{c}","status":"closed"}},
          {{"id":"tp-3","created":"2000-01-01T00:00:00Z","status":"open"}}
        ]"#, c = now.to_rfc3339());
        let b = partition_beads(&json, start, end).unwrap();
        assert_eq!(b.opened, vec!["tp-1"]);
        assert_eq!(b.closed, vec!["tp-2"]);
    }
}
