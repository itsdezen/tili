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
}
