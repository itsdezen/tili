use crate::{Command, Direction, LayoutKind};

/// Parses a keybinding's command string (the second argument of a KDL
/// `bind "key" "command"` line, e.g. `"focus left"`) into a `Command`.
/// Infallible by design: an unrecognized string becomes `Command::Raw`
/// rather than a parse error, so a config referencing a not-yet-implemented
/// command (or a typo) still loads — `dispatch` reports "not implemented"
/// for anything it doesn't handle, which is a better failure mode for a
/// hotkey than refusing to start the daemon.
pub fn parse(command: &str) -> Command {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    match tokens.as_slice() {
        ["focus", dir] => direction(dir).map_or_else(|| raw(&tokens), Command::Focus),
        ["move", dir] => direction(dir).map_or_else(|| raw(&tokens), Command::Move),
        ["workspace", name] => Command::WorkspaceSwitch((*name).to_string()),
        ["move-node-to-workspace", name] => Command::MoveNodeToWorkspace((*name).to_string()),
        ["layout", "toggle"] => Command::LayoutToggle,
        ["layout", "tiles"] => Command::LayoutSet(LayoutKind::Tiles),
        ["layout", "accordion"] => Command::LayoutSet(LayoutKind::Accordion),
        ["resize", amount] => amount
            .parse::<f32>()
            .map_or_else(|_| raw(&tokens), |amount| Command::ResizeRatio { amount }),
        ["mode", "exit"] => Command::ModeExit,
        ["mode", name] => Command::ModeEnter((*name).to_string()),
        ["list-windows"] => Command::ListWindows,
        ["list-workspaces"] => Command::ListWorkspaces,
        ["focus-monitor"] => Command::FocusMonitor,
        ["list-monitors"] => Command::ListMonitors,
        ["reload-config"] => Command::ReloadConfig,
        ["ping"] => Command::Ping,
        _ => raw(&tokens),
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
        assert!(matches!(parse("layout toggle"), Command::LayoutToggle));
        assert!(matches!(
            parse("layout tiles"),
            Command::LayoutSet(LayoutKind::Tiles)
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
    fn unrecognized_command_becomes_raw_not_an_error() {
        match parse("some-future-command with args") {
            Command::Raw { verb, args } => {
                assert_eq!(verb, "some-future-command");
                assert_eq!(args, vec!["with", "args"]);
            }
            other => panic!("expected Raw, got {other:?}"),
        }
    }
}
