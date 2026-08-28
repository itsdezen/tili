mod schema;
mod watch;

pub use schema::{
    AnimationSpeed, Config, ConfigError, FloatingDefaults, FloatingRule, FloatingRuleMode, Gaps,
    Keybinding, KeybindingMode, MenubarConfig, MenubarFill, MenubarShape, Settings,
    WorkspaceConfig, default_config_path, load, parse,
};
pub use watch::spawn_config_watcher;
