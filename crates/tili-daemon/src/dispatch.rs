use tili_ipc::{Command, Response};

use crate::state::WmState;

pub fn dispatch(state: &mut WmState, command: Command) -> Response {
    // Resolves `WmState`'s focus bookkeeping against whatever window real
    // macOS currently considers focused, synchronously, before *any*
    // command runs — see `WmState::sync_focus_from_frontmost`'s doc comment
    // for why this can't be a reactive background sync instead (an
    // unavoidable race against the very next hotkey press). Other AX-based
    // tiling WMs resolve this the same way: a synchronous focus resync at
    // the top of every command, not a reactive background sync.
    state.sync_focus_from_frontmost();
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
        Command::Join(dir) => result_response(state.join(to_tree_direction(dir))),
        Command::ResizeRatio { amount } => result_response(state.resize(amount)),
        Command::OrientationSet(kind, root) => {
            result_response(state.set_orientation(to_tree_orientation(kind), root))
        }
        Command::OrientationToggle(root) => result_response(state.toggle_orientation(root)),
        Command::ListWorkspaces => match serde_json::to_value(state.list_workspaces()) {
            Ok(payload) => Response::OkWithPayload(payload),
            Err(e) => Response::Err {
                message: e.to_string(),
            },
        },
        Command::WorkspaceSwitch(name) => result_response(state.switch_workspace(&name)),
        Command::MoveNodeToWorkspace(name) => {
            result_response(state.move_focused_to_workspace(&name))
        }
        Command::ModeEnter(name) => result_response(state.enter_mode(&name)),
        Command::ModeExit => {
            state.exit_mode();
            Response::Ok
        }
        Command::LayoutToggle(root) => result_response(state.toggle_layout(root)),
        Command::LayoutSet(kind, root) => result_response(state.set_layout(kind, root)),
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
        Command::BalanceSizes { root } => result_response(state.balance_sizes(root)),
        // A distinct `flatten` has no additional effect to implement:
        // `Tree::normalize` already runs after every mutation and already
        // collapses one-child containers — see the refactor plan's own
        // rationale for why this stays a thin no-op rather than exposing a
        // second, redundant normalization entry point.
        Command::Flatten => Response::Ok,
        Command::FullscreenToggle { native } => result_response(state.toggle_fullscreen(native)),
        Command::Close => result_response(state.close_focused()),
        Command::Summon(query) => result_response(state.summon(&query)),
        Command::MoveWorkspaceToMonitor { workspace, target } => {
            result_response(state.move_workspace_to_monitor(workspace.as_deref(), target))
        }
        Command::WorkspaceBack => result_response(state.switch_to_previous_workspace()),
        Command::SetFloating(floating) => result_response(state.set_floating(floating)),
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

fn to_tree_orientation(kind: tili_ipc::OrientationKind) -> tili_tree::Orientation {
    match kind {
        tili_ipc::OrientationKind::Horizontal => tili_tree::Orientation::Horizontal,
        tili_ipc::OrientationKind::Vertical => tili_tree::Orientation::Vertical,
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
        let response = dispatch(&mut state, Command::LayoutToggle(false));
        assert!(matches!(response, Response::Err { .. }));

        let response = dispatch(
            &mut state,
            Command::LayoutSet(tili_ipc::LayoutKind::Accordion, false),
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
    fn switching_to_an_undeclared_workspace_is_an_error() {
        let mut state = WmState::default();
        let response = dispatch(
            &mut state,
            Command::WorkspaceSwitch("entertain".to_string()),
        );
        assert!(matches!(response, Response::Err { .. }));

        let response = dispatch(&mut state, Command::ListWorkspaces);
        let Response::OkWithPayload(payload) = response else {
            panic!("expected OkWithPayload");
        };
        let workspaces: Vec<tili_ipc::WorkspaceInfo> = serde_json::from_value(payload).unwrap();
        assert_eq!(
            workspaces.len(),
            1,
            "undeclared workspace must not be created"
        );
    }

    #[test]
    fn switching_to_a_declared_workspace_activates_it() {
        let mut state = WmState::default();
        let config = tili_config::parse(
            r#"
            workspaces {
                workspace "entertain"
            }
            "#,
        )
        .unwrap();
        state.apply_config(&config);

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
        let active: Vec<_> = workspaces.iter().filter(|w| w.active).collect();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "entertain");
    }

    #[test]
    fn bootstrap_main_workspace_is_dropped_once_real_workspaces_are_declared() {
        let mut state = WmState::default();
        let config = tili_config::parse(
            r#"
            workspaces {
                workspace "work"
                workspace "entertain"
            }
            "#,
        )
        .unwrap();
        state.apply_config(&config);

        let response = dispatch(&mut state, Command::WorkspaceSwitch("main".to_string()));
        assert!(
            matches!(response, Response::Err { .. }),
            "the internal bootstrap workspace must not be reachable once config declares real ones"
        );

        let response = dispatch(&mut state, Command::ListWorkspaces);
        let Response::OkWithPayload(payload) = response else {
            panic!("expected OkWithPayload");
        };
        let workspaces: Vec<tili_ipc::WorkspaceInfo> = serde_json::from_value(payload).unwrap();
        assert!(!workspaces.iter().any(|w| w.name == "main"));
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

    #[test]
    fn flatten_is_always_ok() {
        let mut state = WmState::default();
        let response = dispatch(&mut state, Command::Flatten);
        assert!(matches!(response, Response::Ok));
    }

    #[test]
    fn balance_sizes_with_no_windows_is_an_error() {
        let mut state = WmState::default();
        let response = dispatch(&mut state, Command::BalanceSizes { root: false });
        assert!(matches!(response, Response::Err { .. }));
    }

    #[test]
    fn fullscreen_toggle_with_no_windows_is_an_error() {
        let mut state = WmState::default();
        let response = dispatch(&mut state, Command::FullscreenToggle { native: false });
        assert!(matches!(response, Response::Err { .. }));
    }

    #[test]
    fn close_with_no_windows_is_an_error() {
        let mut state = WmState::default();
        let response = dispatch(&mut state, Command::Close);
        assert!(matches!(response, Response::Err { .. }));
    }

    #[test]
    fn summon_with_no_matching_window_is_an_error() {
        let mut state = WmState::default();
        let response = dispatch(&mut state, Command::Summon("nonexistent".to_string()));
        assert!(matches!(response, Response::Err { .. }));
    }

    #[test]
    fn workspace_back_with_no_previous_workspace_is_an_error() {
        let mut state = WmState::default();
        let response = dispatch(&mut state, Command::WorkspaceBack);
        assert!(matches!(response, Response::Err { .. }));
    }

    #[test]
    fn set_floating_with_no_windows_is_an_error() {
        let mut state = WmState::default();
        let response = dispatch(&mut state, Command::SetFloating(true));
        assert!(matches!(response, Response::Err { .. }));
    }

    #[test]
    fn move_workspace_to_monitor_for_undeclared_workspace_is_an_error() {
        let mut state = WmState::default();
        let response = dispatch(
            &mut state,
            Command::MoveWorkspaceToMonitor {
                workspace: Some("nope".to_string()),
                target: tili_ipc::MonitorTarget::Main,
            },
        );
        assert!(matches!(response, Response::Err { .. }));
    }
}
