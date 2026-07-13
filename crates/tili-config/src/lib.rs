mod schema;
mod watch;

pub use schema::{
    Config, ConfigError, FloatingDefaults, FloatingRule, Gaps, Keybinding, KeybindingMode,
    Settings, WorkspaceConfig, default_config_path, load, parse,
};
pub use watch::spawn_config_watcher;
