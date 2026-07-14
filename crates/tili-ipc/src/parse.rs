use crate::{Command, Direction, LayoutKind, MonitorTarget, OrientationKind, Target};

/// Parses a keybinding's command string (the second argument of a KDL
/// `bind "key" "command"` line, e.g. `"focus left"`) into a `Command`.
/// Infallible by design: an unrecognized string becomes `Command::Raw`
/// rather than a parse error, so a config referencing a not-yet-implemented
/// command (or a typo) still loads — `dispatch` reports "not implemented"
/// for anything it doesn't handle, which is a better failure mode for a
/// hotkey than refusing to start the daemon.
///
/// Discards any `--window-id`/`--workspace` target context — see
/// `parse_with_target` for the variant that keeps it.
pub fn parse(command: &str) -> Command {
    parse_with_target(command).0
}

/// Same parsing as `parse`, but first strips any trailing `--window-id N` /
/// `--workspace NAME` flags off the token list into a `Target`, so a caller
/// (the CLI, or a keybinding) can direct a command at something other than
/// "whatever's currently focused" — see `Target`'s own docs for the
/// precedence `dispatch()` eventually applies. Flags may appear in either
/// order and repeat (a later one of the same kind wins); anything left over
/// after stripping is matched exactly like `parse`.
pub fn parse_with_target(command: &str) -> (Command, Target) {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let (tokens, target) = strip_target_flags(tokens);
    (parse_tokens(&tokens), target)
}

fn strip_target_flags(mut tokens: Vec<&str>) -> (Vec<&str>, Target) {
    let mut target = Target::default();
    loop {
        match tokens.as_slice() {
            [.., "--window-id", value] => {
                if let Ok(id) = value.parse::<u32>() {
                    target.window_id = Some(id);
                }
                tokens.truncate(tokens.len() - 2);
            }
            [.., "--workspace", name] => {
                target.workspace = Some((*name).to_string());
                tokens.truncate(tokens.len() - 2);
            }
            _ => break,
        }
    }
    (tokens, target)
}

fn parse_tokens(tokens: &[&str]) -> Command {
    match tokens {
        ["focus", dir] => direction(dir).map_or_else(|| raw(tokens), Command::Focus),
        ["move", dir] => direction(dir).map_or_else(|| raw(tokens), Command::Move),
        ["join", dir] => direction(dir).map_or_else(|| raw(tokens), Command::Join),
        ["workspace", name] => Command::WorkspaceSwitch((*name).to_string()),
        ["workspace-back"] => Command::WorkspaceBack,
        ["move-node-to-workspace", name] => Command::MoveNodeToWorkspace((*name).to_string()),
        ["move-workspace-to-monitor", target] => monitor_target(target).map_or_else(
            || raw(tokens),
            |target| Command::MoveWorkspaceToMonitor {
                workspace: None,
                target,
            },
        ),
        ["move-workspace-to-monitor", workspace, target] => monitor_target(target).map_or_else(
            || raw(tokens),
            |target| Command::MoveWorkspaceToMonitor {
                workspace: Some((*workspace).to_string()),
                target,
            },
        ),
        ["layout", "toggle"] => Command::LayoutToggle(false),
        ["layout", "toggle", "root"] => Command::LayoutToggle(true),
        ["layout", "tiles"] => Command::LayoutSet(LayoutKind::Tiles, false),
        ["layout", "tiles", "root"] => Command::LayoutSet(LayoutKind::Tiles, true),
        ["layout", "accordion"] => Command::LayoutSet(LayoutKind::Accordion, false),
        ["layout", "accordion", "root"] => Command::LayoutSet(LayoutKind::Accordion, true),
        ["layout", "horizontal"] => Command::OrientationSet(OrientationKind::Horizontal, false),
        ["layout", "horizontal", "root"] => {
            Command::OrientationSet(OrientationKind::Horizontal, true)
        }
        ["layout", "vertical"] => Command::OrientationSet(OrientationKind::Vertical, false),
        ["layout", "vertical", "root"] => Command::OrientationSet(OrientationKind::Vertical, true),
        ["layout", "toggle-orientation"] => Command::OrientationToggle(false),
        ["layout", "toggle-orientation", "root"] => Command::OrientationToggle(true),
        ["resize", amount] => amount
            .parse::<f32>()
            .map_or_else(|_| raw(tokens), |amount| Command::ResizeRatio { amount }),
        ["balance"] => Command::BalanceSizes { root: false },
        ["balance", "root"] => Command::BalanceSizes { root: true },
        ["flatten"] => Command::Flatten,
        ["fullscreen"] => Command::FullscreenToggle { native: false },
        ["fullscreen", "native"] => Command::FullscreenToggle { native: true },
        ["close"] => Command::Close,
        ["summon", name] => Command::Summon((*name).to_string()),
        ["set-floating", "on"] => Command::SetFloating(true),
        ["set-floating", "off"] => Command::SetFloating(false),
        ["mode", "exit"] => Command::ModeExit,
        ["mode", name] => Command::ModeEnter((*name).to_string()),
        ["list-windows"] => Command::ListWindows,
        ["list-workspaces"] => Command::ListWorkspaces,
        ["focus-monitor"] => Command::FocusMonitor,
        ["list-monitors"] => Command::ListMonitors,
        ["reload-config"] => Command::ReloadConfig,
        ["shutdown"] => Command::Shutdown,
        ["ping"] => Command::Ping,
        _ => raw(tokens),
    }
}

fn monitor_target(s: &str) -> Option<MonitorTarget> {
    match s {
        "next" => Some(MonitorTarget::Next),
        "main" => Some(MonitorTarget::Main),
        _ => s.parse::<u32>().ok().map(MonitorTarget::Id),
    }
}

fn direction(s: &str) -> Option<Direction> {
    match s {
        "left" => Some(Direction::Left),
        "right" => Some(Direction::Right),
        "up" => Some(Direction::Up),
        "down" => Some(Direction::Down),
        _ => None,
    }
}

fn raw(tokens: &[&str]) -> Command {
    let mut iter = tokens.iter();
    let verb = iter.next().unwrap_or(&"").to_string();
    let args = iter.map(|s| (*s).to_string()).collect();
    Command::Raw { verb, args }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_focus_and_move() {
        assert!(matches!(
            parse("focus left"),
            Command::Focus(Direction::Left)
        ));
        assert!(matches!(parse("move down"), Command::Move(Direction::Down)));
    }

    #[test]
    fn parses_workspace_commands() {
        assert!(matches!(
            parse("workspace entertain"),
            Command::WorkspaceSwitch(name) if name == "entertain"
        ));
        assert!(matches!(
            parse("move-node-to-workspace random"),
            Command::MoveNodeToWorkspace(name) if name == "random"
        ));
    }

    #[test]
    fn parses_layout_commands() {
        assert!(matches!(
            parse("layout toggle"),
            Command::LayoutToggle(false)
        ));
        assert!(matches!(
            parse("layout toggle root"),
            Command::LayoutToggle(true)
        ));
        assert!(matches!(
            parse("layout tiles"),
            Command::LayoutSet(LayoutKind::Tiles, false)
        ));
        assert!(matches!(
            parse("layout accordion root"),
            Command::LayoutSet(LayoutKind::Accordion, true)
        ));
    }

    #[test]
    fn parses_join_and_orientation_commands() {
        assert!(matches!(parse("join left"), Command::Join(Direction::Left)));
        assert!(matches!(
            parse("layout horizontal"),
            Command::OrientationSet(OrientationKind::Horizontal, false)
        ));
        assert!(matches!(
            parse("layout vertical root"),
            Command::OrientationSet(OrientationKind::Vertical, true)
        ));
    }

    #[test]
    fn parses_orientation_toggle() {
        assert!(matches!(
            parse("layout toggle-orientation"),
            Command::OrientationToggle(false)
        ));
        assert!(matches!(
            parse("layout toggle-orientation root"),
            Command::OrientationToggle(true)
        ));
    }

    #[test]
    fn parses_mode_commands() {
        assert!(matches!(
            parse("mode resize"),
            Command::ModeEnter(name) if name == "resize"
        ));
        assert!(matches!(parse("mode exit"), Command::ModeExit));
    }

    #[test]
    fn parses_shutdown() {
        assert!(matches!(parse("shutdown"), Command::Shutdown));
    }

    #[test]
    fn unrecognized_command_becomes_raw_not_an_error() {
        match parse("some-future-command with args") {
            Command::Raw { verb, args } => {
                assert_eq!(verb, "some-future-command");
                assert_eq!(args, vec!["with", "args"]);
            }
            other => panic!("expected Raw, got {other:?}"),
        }
    }

    #[test]
    fn parses_balance_sizes() {
        assert!(matches!(
            parse("balance"),
            Command::BalanceSizes { root: false }
        ));
        assert!(matches!(
            parse("balance root"),
            Command::BalanceSizes { root: true }
        ));
    }

    #[test]
    fn parses_flatten() {
        assert!(matches!(parse("flatten"), Command::Flatten));
    }

    #[test]
    fn parses_fullscreen_toggle() {
        assert!(matches!(
            parse("fullscreen"),
            Command::FullscreenToggle { native: false }
        ));
        assert!(matches!(
            parse("fullscreen native"),
            Command::FullscreenToggle { native: true }
        ));
    }

    #[test]
    fn parses_close() {
        assert!(matches!(parse("close"), Command::Close));
    }

    #[test]
    fn parses_summon() {
        assert!(matches!(
            parse("summon Slack"),
            Command::Summon(name) if name == "Slack"
        ));
    }

    #[test]
    fn parses_move_workspace_to_monitor() {
        assert!(matches!(
            parse("move-workspace-to-monitor work next"),
            Command::MoveWorkspaceToMonitor { workspace, target: MonitorTarget::Next }
                if workspace == Some("work".to_string())
        ));
        assert!(matches!(
            parse("move-workspace-to-monitor work main"),
            Command::MoveWorkspaceToMonitor { workspace, target: MonitorTarget::Main }
                if workspace == Some("work".to_string())
        ));
        assert!(matches!(
            parse("move-workspace-to-monitor work 3"),
            Command::MoveWorkspaceToMonitor { workspace, target: MonitorTarget::Id(3) }
                if workspace == Some("work".to_string())
        ));
    }

    #[test]
    fn parses_move_workspace_to_monitor_without_an_explicit_workspace() {
        assert!(matches!(
            parse("move-workspace-to-monitor next"),
            Command::MoveWorkspaceToMonitor {
                workspace: None,
                target: MonitorTarget::Next
            }
        ));
        assert!(matches!(
            parse("move-workspace-to-monitor main"),
            Command::MoveWorkspaceToMonitor {
                workspace: None,
                target: MonitorTarget::Main
            }
        ));
        assert!(matches!(
            parse("move-workspace-to-monitor 3"),
            Command::MoveWorkspaceToMonitor {
                workspace: None,
                target: MonitorTarget::Id(3)
            }
        ));
    }

    #[test]
    fn move_workspace_to_monitor_with_unparseable_target_becomes_raw() {
        match parse("move-workspace-to-monitor work not-a-target") {
            Command::Raw { verb, args } => {
                assert_eq!(verb, "move-workspace-to-monitor");
                assert_eq!(args, vec!["work", "not-a-target"]);
            }
            other => panic!("expected Raw, got {other:?}"),
        }
    }

    #[test]
    fn parses_workspace_back() {
        assert!(matches!(parse("workspace-back"), Command::WorkspaceBack));
    }

    #[test]
    fn parses_set_floating() {
        assert!(matches!(
            parse("set-floating on"),
            Command::SetFloating(true)
        ));
        assert!(matches!(
            parse("set-floating off"),
            Command::SetFloating(false)
        ));
    }

    #[test]
    fn parse_with_target_strips_window_id_flag() {
        let (cmd, target) = parse_with_target("close --window-id 42");
        assert!(matches!(cmd, Command::Close));
        assert_eq!(target.window_id, Some(42));
        assert_eq!(target.workspace, None);
    }

    #[test]
    fn parse_with_target_strips_workspace_flag() {
        let (cmd, target) = parse_with_target("close --workspace work");
        assert!(matches!(cmd, Command::Close));
        assert_eq!(target.workspace, Some("work".to_string()));
        assert_eq!(target.window_id, None);
    }

    #[test]
    fn parse_with_target_strips_both_flags_in_either_order() {
        let (cmd, target) = parse_with_target("close --window-id 7 --workspace work");
        assert!(matches!(cmd, Command::Close));
        assert_eq!(target.window_id, Some(7));
        assert_eq!(target.workspace, Some("work".to_string()));

        let (cmd, target) = parse_with_target("close --workspace work --window-id 7");
        assert!(matches!(cmd, Command::Close));
        assert_eq!(target.window_id, Some(7));
        assert_eq!(target.workspace, Some("work".to_string()));
    }

    #[test]
    fn parse_with_target_no_flags_yields_default_target() {
        let (cmd, target) = parse_with_target("focus left");
        assert!(matches!(cmd, Command::Focus(Direction::Left)));
        assert_eq!(target, Target::default());
    }

    #[test]
    fn parse_with_target_ignores_a_non_numeric_window_id() {
        let (cmd, target) = parse_with_target("close --window-id not-a-number");
        assert!(matches!(cmd, Command::Close));
        assert_eq!(target.window_id, None);
    }

    #[test]
    fn plain_parse_discards_target_but_still_strips_flags_from_the_verb_match() {
        assert!(matches!(parse("close --window-id 42"), Command::Close));
    }
}
