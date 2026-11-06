mod schema;
mod watch;

pub use schema::{
    AnimationSpeed, Config, ConfigError, DEFAULT_FLOATING_HEIGHT, DEFAULT_FLOATING_WIDTH,
    FloatingDefaults, FloatingRule, FloatingRuleMode, Gaps, Keybinding, KeybindingMode,
    MenubarConfig, MenubarFill, MenubarShape, Settings, WorkspaceConfig, default_config_path, load,
    parse,
};
pub use watch::spawn_config_watcher;
