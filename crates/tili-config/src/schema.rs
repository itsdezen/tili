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
    /// Regex pattern matched against the window title, if present —
    /// compiled by whoever consumes this (`tili-daemon`), not here, so
    /// this crate doesn't need a regex dependency just to hold a string.
    pub title: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub center: Option<bool>,
}

/// Fallback centering/sizing for a floating window that matched a rule
/// with no explicit `width`/`height`/`center` of its own.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct FloatingDefaults {
    pub center: bool,
    /// Fraction of the monitor's usable width/height, e.g. `0.6` = 60%.
    pub width_ratio: f32,
    pub height_ratio: f32,
}

impl Default for FloatingDefaults {
    fn default() -> Self {
        Self {
            center: true,
            width_ratio: 0.6,
            height_ratio: 0.6,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub mouse_follows_focus: bool,
    pub focus_follows_monitor: bool,
    pub auto_reload: bool,
    /// Which declared workspace is active at daemon startup, on the first
    /// config load only — a hot reload never changes what's currently on
    /// screen. `None` falls back to the alphabetically-first declared
    /// workspace; resolving that fallback is `tili-daemon`'s job (a
    /// startup-sequencing concern), not this crate's.
    pub default_workspace: Option<String>,
    /// How many points of a non-visible `Accordion` sibling peek out from
    /// behind the active one, on the side(s) where a sibling exists — `0`
    /// collapses every child to the exact same full frame. Matches
    /// AeroSpace's accordion padding.
    pub accordion_padding: u32,
    /// Orientation a workspace's root container gets when it's created for
    /// its second window (the point a lone-window root actually becomes a
    /// container) — `"horizontal"`, `"vertical"`, or `"auto"` (pick from
    /// the target monitor's aspect ratio). Anything else is treated as
    /// `"auto"`; converting the string to `tili_tree::Orientation` is
    /// `tili-daemon`'s job, since this crate can't depend on `tili-tree`.
    pub default_root_orientation: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mouse_follows_focus: false,
            focus_follows_monitor: false,
            auto_reload: true,
            default_workspace: None,
            accordion_padding: 30,
            default_root_orientation: "auto".to_string(),
        }
    }
}

/// The parsed `tili.kdl` document.
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
    /// Matched in order — first match wins.
    pub floating_rules: Vec<FloatingRule>,
    pub floating_defaults: FloatingDefaults,
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
/// nodes are silently ignored rather than rejected — the schema is meant
/// to grow across milestones without invalidating configs written against
/// an earlier one.
pub fn parse(source: &str) -> Result<Config, ConfigError> {
    let doc: KdlDocument = source
        .parse()
        .map_err(|e: kdl::KdlError| ConfigError::Parse(e.to_string()))?;

    let (floating_rules, floating_defaults) = parse_floating_rules(&doc);
    let mut config = Config {
        workspaces: parse_workspaces(&doc),
        default_layout: parse_default_layout(&doc),
        settings: parse_settings(&doc),
        keybindings: parse_keybindings(&doc),
        floating_rules,
        floating_defaults,
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

/// `keybindings mode="main" { bind "alt-h" "focus left" }` — multiple
/// sibling `keybindings` nodes can share the name with different `mode`
/// values, so this walks *every* top-level node named `keybindings` rather
/// than using `doc.get`, which would only find the first.
fn parse_keybindings(doc: &KdlDocument) -> Vec<KeybindingMode> {
    doc.nodes()
        .iter()
        .filter(|n| n.name().value() == "keybindings")
        .filter_map(|n| {
            let mode_name = n.get("mode").and_then(|v| v.as_string())?.to_string();
            let children = n.children()?;
            let bindings = children
                .nodes()
                .iter()
                .filter(|b| b.name().value() == "bind")
                .filter_map(|b| {
                    let key = b.get(0)?.as_string()?.to_string();
                    let command = b.get(1)?.as_string()?.to_string();
                    Some(Keybinding { key, command })
                })
                .collect();
            Some(KeybindingMode {
                name: mode_name,
                bindings,
            })
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
    if let Some(v) = children
        .get_arg("default-workspace")
        .and_then(|v| v.as_string())
    {
        settings.default_workspace = Some(v.to_string());
    }
    if let Some(v) = children
        .get_arg("accordion-padding")
        .and_then(|v| v.as_integer())
        .and_then(|v| u32::try_from(v).ok())
    {
        settings.accordion_padding = v;
    }
    if let Some(v) = children
        .get_arg("default-root-orientation")
        .and_then(|v| v.as_string())
    {
        settings.default_root_orientation = v.to_string();
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

/// `floating-rules { rule app-id="..." title="..." { width N; height N;
/// center #bool } ... defaults { center #bool; width-ratio F; height-ratio
/// F } }`. Rules are returned in document order — callers should match
/// first-wins.
fn parse_floating_rules(doc: &KdlDocument) -> (Vec<FloatingRule>, FloatingDefaults) {
    let Some(children) = doc.get("floating-rules").and_then(KdlNode::children) else {
        return (Vec::new(), FloatingDefaults::default());
    };

    let rules = children
        .nodes()
        .iter()
        .filter(|n| n.name().value() == "rule")
        .filter_map(|n| {
            let app_id = n.get("app-id")?.as_string()?.to_string();
            let title = n
                .get("title")
                .and_then(|v| v.as_string())
                .map(str::to_string);
            let (width, height, center) = match n.children() {
                Some(rule_children) => (
                    rule_children
                        .get_arg("width")
                        .and_then(|v| v.as_integer())
                        .and_then(|v| u32::try_from(v).ok()),
                    rule_children
                        .get_arg("height")
                        .and_then(|v| v.as_integer())
                        .and_then(|v| u32::try_from(v).ok()),
                    rule_children.get_arg("center").and_then(|v| v.as_bool()),
                ),
                None => (None, None, None),
            };
            Some(FloatingRule {
                app_id,
                title,
                width,
                height,
                center,
            })
        })
        .collect();

    let mut defaults = FloatingDefaults::default();
    if let Some(defaults_children) = children.get("defaults").and_then(KdlNode::children) {
        if let Some(v) = defaults_children
            .get_arg("center")
            .and_then(|v| v.as_bool())
        {
            defaults.center = v;
        }
        if let Some(v) = as_f32(defaults_children.get_arg("width-ratio")) {
            defaults.width_ratio = v;
        }
        if let Some(v) = as_f32(defaults_children.get_arg("height-ratio")) {
            defaults.height_ratio = v;
        }
    }

    (rules, defaults)
}

/// KDL stores `0.6` and `1` as different internal number variants; a
/// float-ratio setting should accept either rather than silently keeping
/// its default just because someone wrote a whole number.
fn as_f32(value: Option<&kdl::KdlValue>) -> Option<f32> {
    value.and_then(|v| {
        v.as_float()
            .map(|f| f as f32)
            .or_else(|| v.as_integer().map(|i| i as f32))
    })
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
                default-workspace "work"
            }
        "#;
        let config = parse(source).unwrap();
        assert_eq!(config.default_layout.as_deref(), Some("accordion"));
        assert!(config.settings.mouse_follows_focus);
        assert!(!config.settings.auto_reload);
        assert_eq!(config.settings.default_workspace.as_deref(), Some("work"));
        // Untouched setting keeps its default.
        assert!(!config.settings.focus_follows_monitor);
    }

    #[test]
    fn parses_accordion_padding_and_root_orientation() {
        let source = r#"
            settings {
                accordion-padding 40
                default-root-orientation "vertical"
            }
        "#;
        let config = parse(source).unwrap();
        assert_eq!(config.settings.accordion_padding, 40);
        assert_eq!(config.settings.default_root_orientation, "vertical");
    }

    #[test]
    fn accordion_padding_and_root_orientation_default_when_unset() {
        let config = parse("").unwrap();
        assert_eq!(config.settings.accordion_padding, 30);
        assert_eq!(config.settings.default_root_orientation, "auto");
    }

    #[test]
    fn unrecognized_sections_are_ignored_not_rejected() {
        let source = r#"
            experimental-future-section {
                whatever "this-does-not-exist-yet"
            }
            workspaces {
                workspace "work"
            }
        "#;
        let config = parse(source).unwrap();
        assert_eq!(config.workspaces.len(), 1);
    }

    #[test]
    fn parses_floating_rules_and_defaults() {
        let source = r#"
            floating-rules {
                rule app-id="com.apple.finder"
                rule app-id="com.apple.systempreferences" {
                    width 900
                    height 600
                    center #true
                }
                rule app-id="com.apple.finder" title="^Copy$"

                defaults {
                    center #true
                    width-ratio 0.6
                    height-ratio 0.6
                }
            }
        "#;
        let config = parse(source).unwrap();
        assert_eq!(config.floating_rules.len(), 3);

        let plain = &config.floating_rules[0];
        assert_eq!(plain.app_id, "com.apple.finder");
        assert_eq!(plain.width, None);

        let sized = &config.floating_rules[1];
        assert_eq!(sized.app_id, "com.apple.systempreferences");
        assert_eq!(sized.width, Some(900));
        assert_eq!(sized.height, Some(600));
        assert_eq!(sized.center, Some(true));

        let titled = &config.floating_rules[2];
        assert_eq!(titled.title.as_deref(), Some("^Copy$"));

        assert!(config.floating_defaults.center);
        assert!((config.floating_defaults.width_ratio - 0.6).abs() < f32::EPSILON);
        assert!((config.floating_defaults.height_ratio - 0.6).abs() < f32::EPSILON);
    }

    #[test]
    fn floating_defaults_fall_back_when_block_is_absent() {
        let config = parse("").unwrap();
        assert!(config.floating_defaults.center);
        assert!((config.floating_defaults.width_ratio - 0.6).abs() < f32::EPSILON);
    }

    #[test]
    fn parses_keybindings_across_multiple_modes() {
        let source = r#"
            keybindings mode="main" {
                bind "alt-h" "focus left"
                bind "alt-shift-semicolon" "mode resize"
            }
            keybindings mode="resize" {
                bind "h" "resize -50"
                bind "escape" "mode main"
            }
        "#;
        let config = parse(source).unwrap();
        assert_eq!(config.keybindings.len(), 2);

        let main = config
            .keybindings
            .iter()
            .find(|m| m.name == "main")
            .unwrap();
        assert_eq!(main.bindings.len(), 2);
        assert_eq!(main.bindings[0].key, "alt-h");
        assert_eq!(main.bindings[0].command, "focus left");

        let resize = config
            .keybindings
            .iter()
            .find(|m| m.name == "resize")
            .unwrap();
        assert_eq!(resize.bindings.len(), 2);
        assert_eq!(resize.bindings[1].key, "escape");
        assert_eq!(resize.bindings[1].command, "mode main");
    }

    #[test]
    fn load_missing_file_returns_default_config() {
        let config = load(std::path::Path::new("/nonexistent/tili-test.kdl")).unwrap();
        assert!(config.workspaces.is_empty());
    }
}
