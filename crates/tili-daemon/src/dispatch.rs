use tili_ipc::{Command, Response};

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
        _ => Response::Err {
            message: "not implemented yet".to_string(),
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
