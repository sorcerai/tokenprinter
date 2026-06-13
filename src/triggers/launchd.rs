/// Generate a macOS launchd plist XML string for the tokenprinter watch daemon.
///
/// The resulting plist uses:
/// - `ProgramArguments`: `[bin, "watch", "--idle", "<idle_seconds>"]`
/// - `RunAtLoad`: true
/// - `KeepAlive`: true
pub fn launchd_plist(bin: &str, label: &str, idle_seconds: u64) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>watch</string>
        <string>--idle</string>
        <string>{idle_seconds}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/tmp/{label}.stdout.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/{label}.stderr.log</string>
</dict>
</plist>
"#,
        label = label,
        bin = bin,
        idle_seconds = idle_seconds
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launchd_plist_contains_required_fields() {
        let bin = "/usr/local/bin/tokenprinter";
        let label = "com.tokenprinter.watch";
        let xml = launchd_plist(bin, label, 90);

        assert!(
            xml.contains("<key>ProgramArguments</key>"),
            "missing ProgramArguments key"
        );
        assert!(xml.contains(bin), "missing bin path");
        assert!(xml.contains("<string>watch</string>"), "missing watch string");
        assert!(xml.contains(label), "missing label");
        assert!(
            xml.contains("<key>RunAtLoad</key>"),
            "missing RunAtLoad key"
        );
        assert!(xml.contains("<true/>"), "RunAtLoad must be true");
        assert!(
            xml.contains("<key>KeepAlive</key>"),
            "missing KeepAlive key"
        );
        assert!(xml.contains("90"), "missing idle_seconds value");
    }

    #[test]
    fn launchd_plist_is_valid_xml_structure() {
        let xml = launchd_plist("/bin/tokenprinter", "com.example.test", 120);
        // Basic structural checks
        assert!(xml.starts_with("<?xml"), "must start with XML declaration");
        assert!(xml.contains("<plist"), "must contain plist element");
        assert!(xml.contains("</plist>"), "must have closing plist");
        assert!(xml.contains("--idle"), "must include --idle flag");
        assert!(xml.contains("120"), "must include idle value");
    }
}
