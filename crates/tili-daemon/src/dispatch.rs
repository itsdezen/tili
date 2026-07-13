use tili_ipc::{Command, Response};

use crate::state::WmState;

pub fn dispatch(state: &mut WmState, command: Command) -> Response {
    match command {
        Command::Ping => Response::Ok,
        Command::ListWindows => match serde_json::to_value(state.list_windows()) {
            Ok(payload) => Response::OkWithPayload(payload),
            Err(e) => Response::Err {
                message: e.to_string(),
            },
        },
        Command::Focus(dir) => result_response(state.focus(to_tree_direction(dir))),
        Command::Move(dir) => result_response(state.move_focused(to_tree_direction(dir))),
        Command::ListWorkspaces => match serde_json::to_value(state.list_workspaces()) {
            Ok(payload) => Response::OkWithPayload(payload),
            Err(e) => Response::Err {
                message: e.to_string(),
            },
        },
        Command::WorkspaceSwitch(name) => {
            state.switch_workspace(&name);
            Response::Ok
        }
        Command::MoveNodeToWorkspace(name) => {
            result_response(state.move_focused_to_workspace(&name))
        }
        Command::ModeEnter(name) => result_response(state.enter_mode(&name)),
        Command::ModeExit => {
            state.exit_mode();
            Response::Ok
        }
        Command::LayoutToggle => result_response(state.toggle_layout()),
        Command::LayoutSet(kind) => result_response(state.set_layout(kind)),
        Command::FocusMonitor => {
            state.focus_monitor_next();
            Response::Ok
        }
        Command::ListMonitors => match serde_json::to_value(state.list_monitors()) {
            Ok(payload) => Response::OkWithPayload(payload),
            Err(e) => Response::Err {
                message: e.to_string(),
            },
        },
        _ => Response::Err {
            message: "not implemented yet".to_string(),
        },
    }
}

fn to_tree_direction(dir: tili_ipc::Direction) -> tili_tree::Direction {
    match dir {
        tili_ipc::Direction::Left => tili_tree::Direction::Left,
        tili_ipc::Direction::Right => tili_tree::Direction::Right,
        tili_ipc::Direction::Up => tili_tree::Direction::Up,
        tili_ipc::Direction::Down => tili_tree::Direction::Down,
    }
}

fn result_response(result: Result<(), String>) -> Response {
    match result {
        Ok(()) => Response::Ok,
        Err(message) => Response::Err { message },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_returns_ok() {
        let mut state = WmState::default();
        let response = dispatch(&mut state, Command::Ping);
        assert!(matches!(response, Response::Ok));
    }

    #[test]
    fn list_windows_reflects_cache_not_a_fresh_scan() {
        let mut state = WmState::default();
        let response = dispatch(&mut state, Command::ListWindows);
        let Response::OkWithPayload(payload) = response else {
            panic!("expected OkWithPayload");
        };
        let windows: Vec<tili_ipc::WindowInfo> = serde_json::from_value(payload).unwrap();
        assert!(windows.is_empty(), "empty cache before any WmEvent arrives");
    }

    #[test]
    fn focus_with_no_windows_is_an_error() {
        let mut state = WmState::default();
        let response = dispatch(&mut state, Command::Focus(tili_ipc::Direction::Left));
        assert!(matches!(response, Response::Err { .. }));
    }

    #[test]
    fn layout_toggle_with_no_windows_is_an_error() {
        let mut state = WmState::default();
        let response = dispatch(&mut state, Command::LayoutToggle);
        assert!(matches!(response, Response::Err { .. }));

        let response = dispatch(
            &mut state,
            Command::LayoutSet(tili_ipc::LayoutKind::Accordion),
        );
        assert!(matches!(response, Response::Err { .. }));
    }

    #[test]
    fn list_workspaces_starts_with_one_active_default() {
        let mut state = WmState::default();
        let response = dispatch(&mut state, Command::ListWorkspaces);
        let Response::OkWithPayload(payload) = response else {
            panic!("expected OkWithPayload");
        };
        let workspaces: Vec<tili_ipc::WorkspaceInfo> = serde_json::from_value(payload).unwrap();
        assert_eq!(workspaces.len(), 1);
        assert!(workspaces[0].active);
        assert_eq!(workspaces[0].window_count, 0);
    }

    #[test]
    fn switching_to_a_new_workspace_creates_it() {
        let mut state = WmState::default();
        let response = dispatch(
            &mut state,
            Command::WorkspaceSwitch("entertain".to_string()),
        );
        assert!(matches!(response, Response::Ok));

        let response = dispatch(&mut state, Command::ListWorkspaces);
        let Response::OkWithPayload(payload) = response else {
            panic!("expected OkWithPayload");
        };
        let workspaces: Vec<tili_ipc::WorkspaceInfo> = serde_json::from_value(payload).unwrap();
        assert_eq!(workspaces.len(), 2);
        let active: Vec<_> = workspaces.iter().filter(|w| w.active).collect();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "entertain");
    }

    #[test]
    fn entering_an_unknown_mode_is_an_error() {
        let mut state = WmState::default();
        let response = dispatch(&mut state, Command::ModeEnter("resize".to_string()));
        assert!(matches!(response, Response::Err { .. }));
    }

    #[test]
    fn mode_round_trips_via_config_then_hotkey_resolution() {
        let mut state = WmState::default();
        let config = tili_config::parse(
            r#"
            keybindings mode="main" {
                bind "alt-shift-semicolon" "mode resize"
            }
            keybindings mode="resize" {
                bind "escape" "mode main"
            }
            "#,
        )
        .unwrap();
        state.apply_config(&config);

        let enter_resize = tili_ax::parse_key_combo("alt-shift-semicolon").unwrap();
        assert!(state.active_key_combos().contains(&enter_resize));

        let response = dispatch(&mut state, Command::ModeEnter("resize".to_string()));
        assert!(matches!(response, Response::Ok));

        let exit_resize = tili_ax::parse_key_combo("escape").unwrap();
        assert!(state.active_key_combos().contains(&exit_resize));
        assert!(!state.active_key_combos().contains(&enter_resize));
    }
}
