use tili_ipc::{Command, RectInfo, Response, WindowInfo};

/// Holds the daemon's entire mutable state. Both the socket handler and the
/// global-hotkey handler call `dispatch` against the same `WmState`, so
/// CLI-invoked and hotkey-invoked commands can never behave differently.
#[derive(Default)]
pub struct WmState {
    // TODO(M3+): tree per workspace, monitors, focus state.
}

pub fn dispatch(_state: &mut WmState, command: Command) -> Response {
    match command {
        Command::Ping => Response::Ok,
        Command::ListWindows => list_windows(),
        _ => Response::Err {
            message: "not implemented yet".to_string(),
        },
    }
}

fn list_windows() -> Response {
    let windows: Vec<WindowInfo> = tili_ax::list_windows()
        .iter()
        .map(|w| {
            let frame = w.frame();
            WindowInfo {
                id: w.id(),
                pid: w.pid(),
                title: w.title().to_string(),
                frame: RectInfo {
                    x: frame.x,
                    y: frame.y,
                    width: frame.width,
                    height: frame.height,
                },
            }
        })
        .collect();

    match serde_json::to_value(windows) {
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
}
