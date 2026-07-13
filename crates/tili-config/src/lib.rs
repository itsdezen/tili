use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    pub name: String,
    pub monitor: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Gaps {
    pub inner: u32,
    pub outer: (u32, u32, u32, u32),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keybinding {
    pub key: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeybindingMode {
    pub name: String,
    pub bindings: Vec<Keybinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FloatingRule {
    pub app_id: String,
    pub title: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub center: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub mouse_follows_focus: bool,
    pub focus_follows_monitor: bool,
    pub auto_reload: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mouse_follows_focus: false,
            focus_follows_monitor: false,
            auto_reload: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub workspaces: Vec<WorkspaceConfig>,
    pub default_layout: Option<String>,
    pub gaps: Gaps,
    pub settings: Settings,
    pub keybindings: Vec<KeybindingMode>,
    pub floating_rules: Vec<FloatingRule>,
}

#[derive(Debug)]
pub enum ConfigError {
    Parse(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Parse(msg) => write!(f, "config parse error: {msg}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Default config file location: `~/.config/tili/tili.kdl`.
pub fn default_config_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    std::path::PathBuf::from(home).join(".config/tili/tili.kdl")
}

/// Parses a KDL config document into a `Config`. Schema parsing itself lands in M5;
/// for now this validates the document is at least well-formed KDL.
pub fn parse(source: &str) -> Result<Config, ConfigError> {
    kdl::KdlDocument::parse(source).map_err(|e| ConfigError::Parse(e.to_string()))?;
    Ok(Config::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_document_parses_to_default_config() {
        let config = parse("").unwrap();
        assert!(config.workspaces.is_empty());
    }

    #[test]
    fn malformed_kdl_is_rejected() {
        assert!(parse("workspaces {").is_err());
    }
}
