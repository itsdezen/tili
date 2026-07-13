use std::collections::HashMap;

use kdl::{KdlDocument, KdlNode};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    pub name: String,
    pub monitor: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Gaps {
    pub inner: u32,
    /// CSS-shorthand ordered: (top, right, bottom, left).
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

/// The parsed `tili.kdl` document. `keybindings` and `floating_rules` are
/// defined here (they're part of the target schema) but not yet populated
/// from KDL — parsing them is M6 and M8's job respectively; until then they
/// just stay empty, matching the "don't fill in code ahead of its
/// milestone" scope discipline documented in CLAUDE.md.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub workspaces: Vec<WorkspaceConfig>,
    pub default_layout: Option<String>,
    pub gaps: Gaps,
    /// Per-workspace gap overrides, keyed by workspace name — see the
    /// `gaps { workspace "name" { ... } }` KDL shape.
    pub workspace_gaps: HashMap<String, Gaps>,
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

/// Reads and parses the config file at `path`. A missing file is *not* an
/// error — it's treated as "no config yet," returning `Config::default()`
/// so the daemon has sensible behavior on a fresh install rather than
/// requiring a config file to exist before it'll even start.
pub fn load(path: &std::path::Path) -> Result<Config, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(source) => parse(&source),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(ConfigError::Parse(e.to_string())),
    }
}

/// Parses a KDL config document into a `Config`. Unrecognized top-level
/// nodes (e.g. a `keybindings` block, ahead of M6) are silently ignored
/// rather than rejected — the schema is meant to grow across milestones
/// without invalidating configs written against an earlier one.
pub fn parse(source: &str) -> Result<Config, ConfigError> {
    let doc: KdlDocument = source
        .parse()
        .map_err(|e: kdl::KdlError| ConfigError::Parse(e.to_string()))?;

    let mut config = Config {
        workspaces: parse_workspaces(&doc),
        default_layout: parse_default_layout(&doc),
        settings: parse_settings(&doc),
        ..Config::default()
    };
    let (gaps, workspace_gaps) = parse_gaps(&doc);
    config.gaps = gaps;
    config.workspace_gaps = workspace_gaps;

    Ok(config)
}

fn parse_default_layout(doc: &KdlDocument) -> Option<String> {
    doc.get_arg("default-layout")
        .and_then(|v| v.as_string())
        .map(str::to_string)
}

fn parse_workspaces(doc: &KdlDocument) -> Vec<WorkspaceConfig> {
    let Some(children) = doc.get("workspaces").and_then(KdlNode::children) else {
        return Vec::new();
    };
    children
        .nodes()
        .iter()
        .filter(|n| n.name().value() == "workspace")
        .filter_map(|n| {
            let name = n.get(0)?.as_string()?.to_string();
            let monitor = n
                .get("monitor")
                .and_then(|v| v.as_string())
                .map(str::to_string);
            Some(WorkspaceConfig { name, monitor })
        })
        .collect()
}

fn parse_settings(doc: &KdlDocument) -> Settings {
    let mut settings = Settings::default();
    let Some(children) = doc.get("settings").and_then(KdlNode::children) else {
        return settings;
    };
    if let Some(v) = children
        .get_arg("mouse-follows-focus")
        .and_then(|v| v.as_bool())
    {
        settings.mouse_follows_focus = v;
    }
    if let Some(v) = children
        .get_arg("focus-follows-monitor")
        .and_then(|v| v.as_bool())
    {
        settings.focus_follows_monitor = v;
    }
    if let Some(v) = children.get_arg("auto-reload").and_then(|v| v.as_bool()) {
        settings.auto_reload = v;
    }
    settings
}

fn parse_gap_values(node: &KdlNode) -> Gaps {
    let mut gaps = Gaps::default();
    let Some(children) = node.children() else {
        return gaps;
    };
    if let Some(v) = children
        .get_arg("inner")
        .and_then(|v| v.as_integer())
        .and_then(|v| u32::try_from(v).ok())
    {
        gaps.inner = v;
    }
    if let Some(outer_node) = children.get("outer") {
        let values: Vec<u32> = (0..4)
            .filter_map(|i| outer_node.get(i).and_then(|v| v.as_integer()))
            .filter_map(|v| u32::try_from(v).ok())
            .collect();
        gaps.outer = match values.as_slice() {
            [all] => (*all, *all, *all, *all),
            [top, right, bottom, left] => (*top, *right, *bottom, *left),
            _ => gaps.outer,
        };
    }
    gaps
}

fn parse_gaps(doc: &KdlDocument) -> (Gaps, HashMap<String, Gaps>) {
    let Some(gaps_node) = doc.get("gaps") else {
        return (Gaps::default(), HashMap::new());
    };
    let global = parse_gap_values(gaps_node);

    let mut overrides = HashMap::new();
    if let Some(children) = gaps_node.children() {
        for workspace_node in children
            .nodes()
            .iter()
            .filter(|n| n.name().value() == "workspace")
        {
            if let Some(name) = workspace_node.get(0).and_then(|v| v.as_string()) {
                overrides.insert(name.to_string(), parse_gap_values(workspace_node));
            }
        }
    }
    (global, overrides)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_document_parses_to_default_config() {
        let config = parse("").unwrap();
        assert!(config.workspaces.is_empty());
        assert_eq!(config.gaps.inner, 0);
    }

    #[test]
    fn malformed_kdl_is_rejected() {
        assert!(parse("workspaces {").is_err());
    }

    #[test]
    fn parses_workspaces_with_monitor_attribute() {
        let source = r#"
            workspaces {
                workspace "work" monitor="main"
                workspace "random"
            }
        "#;
        let config = parse(source).unwrap();
        assert_eq!(config.workspaces.len(), 2);
        assert_eq!(config.workspaces[0].name, "work");
        assert_eq!(config.workspaces[0].monitor.as_deref(), Some("main"));
        assert_eq!(config.workspaces[1].name, "random");
        assert_eq!(config.workspaces[1].monitor, None);
    }

    #[test]
    fn parses_global_gaps_single_and_four_value_outer() {
        let source = r#"
            gaps {
                inner 4
                outer 8 8 8 8
            }
        "#;
        let config = parse(source).unwrap();
        assert_eq!(config.gaps.inner, 4);
        assert_eq!(config.gaps.outer, (8, 8, 8, 8));
    }

    #[test]
    fn parses_per_workspace_gap_overrides() {
        let source = r#"
            gaps {
                inner 4
                outer 8
                workspace "entertain" {
                    inner 0
                    outer 0
                }
            }
        "#;
        let config = parse(source).unwrap();
        assert_eq!(config.gaps.inner, 4);
        assert_eq!(config.gaps.outer, (8, 8, 8, 8));
        let override_gaps = config.workspace_gaps.get("entertain").unwrap();
        assert_eq!(override_gaps.inner, 0);
        assert_eq!(override_gaps.outer, (0, 0, 0, 0));
    }

    #[test]
    fn parses_settings_and_default_layout() {
        // KDL v2 booleans are `#true`/`#false` (`#`-prefixed keywords, to
        // disambiguate from bare identifiers) — not bare `true`/`false`
        // like v1 or most other config languages. Easy to get wrong when
        // hand-writing a config; see the same note on the example in
        // README.md.
        let source = r#"
            default-layout "accordion"
            settings {
                mouse-follows-focus #true
                auto-reload #false
            }
        "#;
        let config = parse(source).unwrap();
        assert_eq!(config.default_layout.as_deref(), Some("accordion"));
        assert!(config.settings.mouse_follows_focus);
        assert!(!config.settings.auto_reload);
        // Untouched setting keeps its default.
        assert!(!config.settings.focus_follows_monitor);
    }

    #[test]
    fn unrecognized_sections_are_ignored_not_rejected() {
        let source = r#"
            keybindings mode="main" {
                bind "alt-h" "focus left"
            }
            workspaces {
                workspace "work"
            }
        "#;
        let config = parse(source).unwrap();
        assert_eq!(config.workspaces.len(), 1);
        assert!(config.keybindings.is_empty());
    }

    #[test]
    fn load_missing_file_returns_default_config() {
        let config = load(std::path::Path::new("/nonexistent/tili-test.kdl")).unwrap();
        assert!(config.workspaces.is_empty());
    }
}
