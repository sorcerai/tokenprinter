/// Escape a string for safe interpolation into plist XML.
/// Replaces the five special XML characters: & < > " '
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// Generate a macOS launchd plist XML string for the tokenprinter watch daemon.
///
/// The resulting plist uses:
/// - `ProgramArguments`: `[bin, "watch", "--idle", "<idle_seconds>"]`
/// - `RunAtLoad`: true
/// - `KeepAlive`: true
pub fn launchd_plist(bin: &str, label: &str, idle_seconds: u64) -> String {
    let bin_escaped = xml_escape(bin);
    let label_escaped = xml_escape(label);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label_escaped}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin_escaped}</string>
        <string>watch</string>
        <string>--idle</string>
        <string>{idle_seconds}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/tmp/{label_escaped}.stdout.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/{label_escaped}.stderr.log</string>
</dict>
</plist>
"#
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

    #[test]
    fn launchd_plist_xml_escapes_ampersand_in_bin() {
        // A bin path containing '&' must produce well-formed XML: no raw & in output.
        let xml = launchd_plist("/opt/my&tool/tokenprinter", "com.example.test", 60);
        assert!(
            !xml.contains("my&tool"),
            "raw & must not appear in XML output"
        );
        assert!(
            xml.contains("my&amp;tool"),
            "& must be escaped to &amp; in XML output"
        );
    }

    #[test]
    fn xml_escape_handles_all_special_chars() {
        assert_eq!(xml_escape("a&b<c>d\"e'f"), "a&amp;b&lt;c&gt;d&quot;e&apos;f");
        assert_eq!(xml_escape("no specials"), "no specials");
        assert_eq!(xml_escape(""), "");
    }
}
