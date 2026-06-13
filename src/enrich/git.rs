use crate::model::GitStats;
use crate::proc::output_with_timeout;
use chrono::{DateTime, Utc};
use std::path::Path;
use std::process::Command;

/// Commits + diffstat authored in [start, end] in the repo at `dir`.
/// Time-correlated, not causally exact.
pub fn git_stats(dir: &Path, start: DateTime<Utc>, end: DateTime<Utc>) -> anyhow::Result<GitStats> {
    let mut g = GitStats::default();
    if !dir.join(".git").exists() && !is_in_worktree(dir) { return Ok(g); }
    let since = start.to_rfc3339();
    let until = end.to_rfc3339();

    // commit count
    let out = output_with_timeout(
        { let mut c = Command::new("git"); c.current_dir(dir)
            .args(["log", "--since", &since, "--until", &until, "--pretty=%H"]); c },
        10,
    )?;
    g.commits = String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.is_empty()).count() as u32;

    // diffstat across those commits
    let out = output_with_timeout(
        { let mut c = Command::new("git"); c.current_dir(dir)
            .args(["log", "--since", &since, "--until", &until, "--pretty=tformat:", "--numstat"]); c },
        10,
    )?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut files = std::collections::HashSet::new();
    for line in text.lines() {
        let mut it = line.split('\t');
        let (a, d, f) = (it.next(), it.next(), it.next());
        if let (Some(a), Some(d), Some(f)) = (a, d, f) {
            g.added += a.parse::<u32>().unwrap_or(0);
            g.removed += d.parse::<u32>().unwrap_or(0);
            files.insert(f.to_string());
        }
    }
    g.files_changed = files.len() as u32;
    Ok(g)
}

fn is_in_worktree(dir: &Path) -> bool {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(["rev-parse","--is-inside-work-tree"]);
    output_with_timeout(cmd, 10).map(|o| o.status.success()).unwrap_or(false)
}

/// Current branch name, if any.
pub fn current_branch(dir: &Path) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(["rev-parse","--abbrev-ref","HEAD"]);
    let out = output_with_timeout(cmd, 10).ok()?;
    if !out.status.success() { return None; }
    let b = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if b.is_empty() { None } else { Some(b) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Utc, Duration};
    use std::process::Command;

    fn sh(dir: &std::path::Path, args: &[&str]) {
        let ok = Command::new("git").args(args).current_dir(dir)
            .env("GIT_AUTHOR_NAME","t").env("GIT_AUTHOR_EMAIL","t@t")
            .env("GIT_COMMITTER_NAME","t").env("GIT_COMMITTER_EMAIL","t@t")
            .status().unwrap().success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn counts_commits_and_lines_in_window() {
        let dir = std::env::temp_dir().join(format!("tp-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        sh(&dir, &["init","-q"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        sh(&dir, &["add","."]);
        sh(&dir, &["commit","-q","-m","c1"]);

        let start = Utc::now() - Duration::minutes(1);
        let end = Utc::now() + Duration::minutes(1);
        let g = git_stats(&dir, start, end).unwrap();
        assert_eq!(g.commits, 1);
        assert!(g.added >= 2, "added {}", g.added);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
