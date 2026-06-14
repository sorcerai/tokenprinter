use anyhow::Context;
use std::path::Path;

/// Merge tokenprinter hook entries into a Claude settings JSON at `settings_path`.
///
/// Idempotent: will not add an entry if one already containing both "tokenprinter"
/// and the matching command string is present in the array.
pub fn install_hooks(settings_path: &Path, bin: &str) -> anyhow::Result<()> {
    // Read existing settings or start fresh.
    let existing = if settings_path.exists() {
        std::fs::read_to_string(settings_path)
            .with_context(|| format!("reading {}", settings_path.display()))?
    } else {
        "{}".to_string()
    };

    let mut root: serde_json::Value = serde_json::from_str(&existing)
        .with_context(|| format!("parsing {}", settings_path.display()))?;

    // Ensure root is an object.
    if !root.is_object() {
        root = serde_json::json!({});
    }

    // Ensure hooks is an object.
    if !root["hooks"].is_object() {
        root["hooks"] = serde_json::json!({});
    }

    // Unique stable sentinels appended to each command string.
    // Shell treats `# ...` as a comment, so they are harmless when the hook runs.
    // Dedup checks for the exact sentinel string in the serialized JSON entry.
    let sessionend_sentinel = " # tokenprinter-sessionend";
    let precompact_sentinel = " # tokenprinter-precompact";

    // SessionEnd fires once when a Claude session ends — one receipt per session.
    // Note: $CLAUDE_SESSION_ID may be empty in SessionEnd; the --session empty-value
    // fix in cli.rs falls back to newest-by-mtime, which is the session just ended.
    let sessionend_cmd = format!(
        "{bin} print --agent claude --session \"$CLAUDE_SESSION_ID\" --quiet{sessionend_sentinel}"
    );
    let precompact_cmd = format!(
        "{bin} print --agent claude --session \"$CLAUDE_SESSION_ID\" --precompact --quiet{precompact_sentinel}"
    );

    // Helper: check if an entry in the array already contains the exact sentinel for this hook.
    let already_present = |arr: &serde_json::Value, sentinel: &str| -> bool {
        arr.as_array()
            .map(|entries| {
                entries.iter().any(|e| {
                    let s = e.to_string();
                    s.contains(sentinel)
                })
            })
            .unwrap_or(false)
    };

    // Ensure SessionEnd is an array, then maybe append.
    // SessionEnd fires once per session (not per-response like Stop), so we get
    // exactly one receipt per session rather than one per assistant turn.
    if !root["hooks"]["SessionEnd"].is_array() {
        root["hooks"]["SessionEnd"] = serde_json::json!([]);
    }
    if !already_present(&root["hooks"]["SessionEnd"], sessionend_sentinel) {
        let entry = serde_json::json!({
            "hooks": [{"type": "command", "command": sessionend_cmd}]
        });
        root["hooks"]["SessionEnd"].as_array_mut().unwrap().push(entry);
    }

    // Ensure PreCompact is an array, then maybe append.
    if !root["hooks"]["PreCompact"].is_array() {
        root["hooks"]["PreCompact"] = serde_json::json!([]);
    }
    if !already_present(&root["hooks"]["PreCompact"], precompact_sentinel) {
        let entry = serde_json::json!({
            "hooks": [{"type": "command", "command": precompact_cmd}]
        });
        root["hooks"]["PreCompact"].as_array_mut().unwrap().push(entry);
    }

    // Write back pretty-printed.
    if let Some(parent) = settings_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }
    }
    let pretty = serde_json::to_string_pretty(&root).context("serialising settings")?;
    std::fs::write(settings_path, pretty)
        .with_context(|| format!("writing {}", settings_path.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_merges_sessionend_and_precompact_without_clobbering() {
        let dir = std::env::temp_dir().join(format!(
            "tp-hooks-{}-{}",
            std::process::id(),
            1
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        // Pre-populate SessionEnd with an existing (non-tokenprinter) entry.
        std::fs::write(
            &path,
            r#"{"hooks":{"SessionEnd":[{"hooks":[{"type":"command","command":"existing"}]}]},"other":true}"#,
        )
        .unwrap();

        install_hooks(&path, "/usr/local/bin/tokenprinter").unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["other"], serde_json::json!(true));
        let sessionend = v["hooks"]["SessionEnd"].as_array().unwrap();
        assert!(sessionend.iter().any(|e| e.to_string().contains("existing")));
        assert!(sessionend.iter().any(|e| e.to_string().contains("tokenprinter")));
        assert!(v["hooks"]["PreCompact"]
            .to_string()
            .contains("tokenprinter"));

        // idempotent: second run does not duplicate
        install_hooks(&path, "/usr/local/bin/tokenprinter").unwrap();
        let v2: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let n = v2["hooks"]["SessionEnd"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e.to_string().contains("tokenprinter"))
            .count();
        assert_eq!(n, 1, "must not duplicate on re-run");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_creates_file_when_absent() {
        let dir = std::env::temp_dir().join(format!(
            "tp-hooks-{}-absent",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("settings.json");

        install_hooks(&path, "/usr/local/bin/tokenprinter").unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(v["hooks"]["SessionEnd"].to_string().contains("tokenprinter"));
        assert!(v["hooks"]["PreCompact"]
            .to_string()
            .contains("tokenprinter"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sessionend_sentinel_not_blocked_by_precompact_lookalike() {
        // A manually-crafted SessionEnd entry that contains "--precompact" (as a foreign command)
        // but does NOT contain the sessionend sentinel " # tokenprinter-sessionend" must NOT
        // prevent the real SessionEnd hook from being added.
        let dir = std::env::temp_dir().join(format!(
            "tp-hooks-{}-sentinel",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        // Pre-populate SessionEnd with an entry that contains "--precompact" but NOT the sessionend sentinel.
        std::fs::write(
            &path,
            r#"{"hooks":{"SessionEnd":[{"hooks":[{"type":"command","command":"other-tool --precompact --quiet"}]}]}}"#,
        )
        .unwrap();

        install_hooks(&path, "/usr/local/bin/tokenprinter").unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let sessionend = v["hooks"]["SessionEnd"].as_array().unwrap();

        // The real tokenprinter SessionEnd hook must have been added (sentinel is present).
        assert!(
            sessionend.iter().any(|e| e.to_string().contains("tokenprinter-sessionend")),
            "sessionend sentinel must be present even when a lookalike --precompact entry exists"
        );

        // Running again must remain idempotent (still exactly one sessionend-sentinel entry).
        install_hooks(&path, "/usr/local/bin/tokenprinter").unwrap();
        let v2: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let n = v2["hooks"]["SessionEnd"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e.to_string().contains("tokenprinter-sessionend"))
            .count();
        assert_eq!(n, 1, "sessionend sentinel must not duplicate on re-run");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
