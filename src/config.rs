use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub location: String,
    pub paper_width: usize,
    pub idle_seconds: u64,
    pub transport: String,   // auto | cups | usb
    pub queue_name: String,
    pub show_tools: bool,
    pub show_productivity: bool,
    pub show_beads: bool,
    pub show_sparkline: bool,
    pub show_theatrics: bool,
    pub show_qr: bool,
    pub timezone: String,
    /// Billing mode: "subscription" (not charged) or "api" (actual charge).
    pub billing: String,
    /// OpenRouter API key. Falls back to OPENROUTER_API_KEY env var at runtime.
    pub openrouter_key: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            location: "Somewhere on Earth".into(),
            paper_width: 48,
            idle_seconds: 90,
            transport: "auto".into(),
            queue_name: "Star_TSP654".into(),
            show_tools: true, show_productivity: true, show_beads: true,
            show_sparkline: true, show_theatrics: true, show_qr: true,
            timezone: "America/Chicago".into(),
            billing: "subscription".into(),
            openrouter_key: String::new(),
        }
    }
}

impl Config {
    pub fn path() -> std::path::PathBuf {
        dirs::config_dir().unwrap_or_default().join("tokenprinter/config.toml")
    }
    pub fn load() -> Config {
        let p = Self::path();
        match std::fs::read_to_string(&p) {
            Ok(s) => toml::from_str(&s).unwrap_or_default(),
            Err(_) => Config::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_and_toml_override() {
        let c = Config::default();
        assert_eq!(c.paper_width, 48);
        assert_eq!(c.queue_name, "Star_TSP654");
        let toml = r#"
            location = "Edinburgh, Scotland"
            transport = "usb"
            idle_seconds = 120
        "#;
        let c2: Config = toml::from_str(toml).unwrap();
        assert_eq!(c2.location, "Edinburgh, Scotland");
        assert_eq!(c2.transport, "usb");
        assert_eq!(c2.idle_seconds, 120);
        assert_eq!(c2.paper_width, 48); // serde default applied
    }
}
