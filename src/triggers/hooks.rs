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

    let stop_cmd = format!(
        "{} print --agent claude --session \"$CLAUDE_SESSION_ID\" --quiet",
        bin
    );
    let precompact_cmd = format!(
        "{} print --agent claude --session \"$CLAUDE_SESSION_ID\" --precompact --quiet",
        bin
    );

    // Helper: check if an entry in the array already contains a tokenprinter hook
    // matching the given dedup marker (a unique substring of the command).
    // We check the JSON-serialized form of the entry against marker substrings
    // that survive JSON encoding (no special chars).
    let already_present = |arr: &serde_json::Value, marker: &str| -> bool {
        arr.as_array()
            .map(|entries| {
                entries.iter().any(|e| {
                    let s = e.to_string();
                    s.contains("tokenprinter") && s.contains(marker)
                })
            })
            .unwrap_or(false)
    };

    // Dedup markers: substrings that are unique to each hook type and survive JSON encoding.
    // "--precompact" only appears in the PreCompact hook; Stop hook has "--quiet" but not "--precompact".
    // We also need a marker for Stop that distinguishes it from PreCompact.
    let stop_marker = "--quiet";      // present in both; combined with "tokenprinter" check and absence of --precompact via array key
    let precompact_marker = "--precompact";

    // Ensure Stop is an array, then maybe append.
    if !root["hooks"]["Stop"].is_array() {
        root["hooks"]["Stop"] = serde_json::json!([]);
    }
    // For Stop dedup: entry must have tokenprinter AND --quiet AND NOT --precompact
    let stop_already = root["hooks"]["Stop"]
        .as_array()
        .map(|entries| {
            entries.iter().any(|e| {
                let s = e.to_string();
                s.contains("tokenprinter") && s.contains(stop_marker) && !s.contains(precompact_marker)
            })
        })
        .unwrap_or(false);
    if !stop_already {
        let entry = serde_json::json!({
            "hooks": [{"type": "command", "command": stop_cmd}]
        });
        root["hooks"]["Stop"].as_array_mut().unwrap().push(entry);
    }

    // Ensure PreCompact is an array, then maybe append.
    if !root["hooks"]["PreCompact"].is_array() {
        root["hooks"]["PreCompact"] = serde_json::json!([]);
    }
    if !already_present(&root["hooks"]["PreCompact"], precompact_marker) {
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
    fn install_merges_stop_and_precompact_without_clobbering() {
        let dir = std::env::temp_dir().join(format!(
            "tp-hooks-{}-{}",
            std::process::id(),
            1
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"existing"}]}]},"other":true}"#,
        )
        .unwrap();

        install_hooks(&path, "/usr/local/bin/tokenprinter").unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["other"], serde_json::json!(true));
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert!(stop.iter().any(|e| e.to_string().contains("existing")));
        assert!(stop.iter().any(|e| e.to_string().contains("tokenprinter")));
        assert!(v["hooks"]["PreCompact"]
            .to_string()
            .contains("tokenprinter"));

        // idempotent: second run does not duplicate
        install_hooks(&path, "/usr/local/bin/tokenprinter").unwrap();
        let v2: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let n = v2["hooks"]["Stop"]
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
        assert!(v["hooks"]["Stop"].to_string().contains("tokenprinter"));
        assert!(v["hooks"]["PreCompact"]
            .to_string()
            .contains("tokenprinter"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
