use tili_ipc::{Command, Response};

use crate::state::WmState;

pub fn dispatch(state: &mut WmState, command: Command) -> Response {
    let dispatch_start = std::time::Instant::now();
    let command_name = format!("{command:?}");
    let response = dispatch_inner(state, command);
    eprintln!(
        "tili-daemon: dispatch {command_name} took {:?}",
        dispatch_start.elapsed()
    );
    response
}

fn dispatch_inner(state: &mut WmState, command: Command) -> Response {
    // Resolves `WmState`'s focus bookkeeping against whatever window real
    // macOS currently considers focused, synchronously, before *any*
    // command runs — see `WmState::sync_focus_from_frontmost`'s doc comment
    // for why this can't be a reactive background sync instead (an
    // unavoidable race against the very next hotkey press). Other AX-based
    // tiling WMs resolve this the same way: a synchronous focus resync at
    // the top of every command, not a reactive background sync.
    state.sync_focus_from_frontmost();
    // A command reaching here that isn't one of the read-only queries
    // `tili-menubar`'s own long-poll-driven refresh issues is unambiguous
    // proof of real user activity (a hotkey press or an explicit CLI/socket
    // action) — see `WmState::clear_wake_lock`'s doc comment for why that
    // distinction (not just "reached dispatch()") matters.
    if !crate::command_is_read_only(&command) {
        state.clear_wake_lock();
    }
    match command {
        Command::Ping => Response::Ok,
        Command::ListWindows => payload_response(state.list_windows()),
        Command::Focus(dir) => result_response(state.focus(to_tree_direction(dir))),
        Command::Move(dir) => result_response(state.move_focused(to_tree_direction(dir))),
        Command::Join(dir) => result_response(state.join(to_tree_direction(dir))),
        Command::ResizeRatio { amount } => result_response(state.resize(amount)),
        Command::OrientationSet(kind, root) => {
            result_response(state.set_orientation(to_tree_orientation(kind), root))
        }
        Command::OrientationToggle(root) => result_response(state.toggle_orientation(root)),
        Command::ListWorkspaces => payload_response(state.list_workspaces()),
        Command::WorkspaceSwitch(name) => result_response(state.switch_workspace(&name)),
        Command::MoveNodeToWorkspace(name) => {
            result_response(state.move_focused_to_workspace(&name))
        }
        Command::ModeEnter(name) => result_response(state.enter_mode(&name)),
        Command::ModeExit => {
            state.exit_mode();
            Response::Ok
        }
        Command::CurrentMode => {
            Response::OkWithPayload(serde_json::Value::String(state.current_mode().to_string()))
        }
        Command::LayoutToggle(root) => result_response(state.toggle_layout(root)),
        Command::LayoutSet(kind, root) => result_response(state.set_layout(kind, root)),
        Command::FocusMonitor => {
            state.focus_monitor_next();
            Response::Ok
        }
        Command::ListMonitors => payload_response(state.list_monitors()),
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
        Command::Doctor => {
            // Both permission checks are plain, non-prompting reads at this
            // point: the daemon is only alive to answer this at all because
            // it already passed `ensure_accessibility_permission()` once at
            // its own startup (see `main.rs`'s `stop_self` path for what
            // happens when it doesn't), so macOS won't re-prompt for a
            // decision it already has — it just reports the existing grant.
            let report = tili_ipc::DoctorReport {
                accessibility_granted: tili_ax::ensure_accessibility_permission(),
                input_monitoring_granted: tili_ax::has_input_monitoring_permission(),
                config_warnings: state.config_warnings(),
            };
            payload_response(report)
        }
        // `tili-ipc`'s parser deliberately never fails on an unrecognized
        // command string — it becomes `Command::Raw` so a typo'd keybinding
        // still lets the rest of the config load, and fails here instead
        // (see `tili_ipc::parse`'s doc comment). Naming `verb` here, rather
        // than falling into the generic arm below, is what actually
        // surfaces the typo to the user instead of a useless "not
        // implemented yet".
        Command::Raw { verb, .. } => Response::Err {
            message: format!("unknown command: {verb}"),
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

/// Serializes `value` into a `Response`, same shape every `ListWindows`/
/// `ListWorkspaces`/`ListMonitors`/`Doctor` arm needs.
fn payload_response<T: serde::Serialize>(value: T) -> Response {
    match serde_json::to_value(value) {
        Ok(payload) => Response::OkWithPayload(payload),
        Err(e) => Response::Err {
            message: e.to_string(),
        },
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
    fn raw_command_error_names_the_unknown_verb() {
        let mut state = WmState::default();
        let response = dispatch(
            &mut state,
            Command::Raw {
                verb: "fcous".to_string(),
                args: vec!["left".to_string()],
            },
        );
        let Response::Err { message } = response else {
            panic!("expected Response::Err");
        };
        assert!(
            message.contains("fcous"),
            "message should name the unknown verb: {message}"
        );
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
    fn current_mode_reports_active_mode() {
        let mut state = WmState::default();
        let response = dispatch(&mut state, Command::CurrentMode);
        assert!(matches!(
            response,
            Response::OkWithPayload(serde_json::Value::String(ref s)) if s == "main"
        ));

        let config = tili_config::parse(
            r#"
            keybindings mode="manage" auto-exit=#true {
                bind "escape" "mode main"
            }
            "#,
        )
        .unwrap();
        state.apply_config(&config);
        dispatch(&mut state, Command::ModeEnter("manage".to_string()));

        let response = dispatch(&mut state, Command::CurrentMode);
        assert!(matches!(
            response,
            Response::OkWithPayload(serde_json::Value::String(ref s)) if s == "manage"
        ));
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
