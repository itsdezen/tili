use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use core_foundation::runloop::CFRunLoop;
use core_graphics::event::{
    CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, CallbackResult, EventField, KeyCode,
};

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOHIDCheckAccess(request_type: u32) -> u32;
    fn IOHIDRequestAccess(request_type: u32) -> u8;
}

/// `kIOHIDRequestTypeListenEvent` from `<IOKit/hid/IOHIDLib.h>` — the only
/// request type relevant here, since tili only ever listens for key/mouse
/// events, never synthesizes them (`kIOHIDRequestTypePostEvent`).
const IOHID_REQUEST_TYPE_LISTEN_EVENT: u32 = 0;
/// `kIOHIDAccessTypeGranted`.
const IOHID_ACCESS_TYPE_GRANTED: u32 = 0;

/// How long `spawn_hotkey_tap` waits between retrying a failed
/// `CGEventTap` install — e.g. Input Monitoring granted after the daemon
/// already started. Bounded and infrequent enough not to be a busy-loop.
const HOTKEY_TAP_RETRY_INTERVAL: Duration = Duration::from_secs(3);

/// Checks Input Monitoring permission via the public (if under-documented)
/// `IOHIDCheckAccess` API — `CGEventTap` needs this in addition to
/// Accessibility on modern macOS. Never prompts; see
/// `request_input_monitoring_permission` for the one that does.
pub fn has_input_monitoring_permission() -> bool {
    // SAFETY: `IOHIDCheckAccess` takes a plain integer and returns one, no
    // pointers or lifetimes involved.
    unsafe { IOHIDCheckAccess(IOHID_REQUEST_TYPE_LISTEN_EVENT) == IOHID_ACCESS_TYPE_GRANTED }
}

/// Triggers the system Input Monitoring consent dialog via `IOHIDRequestAccess`
/// (distinct from `IOHIDCheckAccess` above, which only checks without
/// prompting) — blocks until the user responds, if a decision hasn't
/// already been made for this exact binary/identity.
///
/// **Must be called before anything calls into Accessibility's
/// `AXIsProcessTrustedWithOptions`** (i.e. before
/// `ensure_accessibility_permission` anywhere in the process) — a
/// long-standing macOS bug
/// (rdar://7381305) means this dialog silently never appears if
/// Accessibility's check has already run once in the same process. Don't
/// reorder call sites without preserving this.
pub fn request_input_monitoring_permission() -> bool {
    // SAFETY: `IOHIDRequestAccess` takes a plain integer and returns a
    // `Boolean` (`u8`), no pointers or lifetimes involved.
    unsafe { IOHIDRequestAccess(IOHID_REQUEST_TYPE_LISTEN_EVENT) != 0 }
}

/// One key combo: modifier keys plus a base key's virtual keycode. Owns no
/// macOS-API types in its public shape — `core_graphics::event::CGEventFlags`
/// stays an implementation detail of this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub cmd: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub shift: bool,
    pub keycode: u16,
}

/// Spawns the global hotkey capture tap on its own dedicated `CFRunLoop`
/// thread. This thread is mandatory regardless of whether the process has
/// an `NSApplication`: `CGEventTap::with_enabled(..., CFRunLoop::run_current)`
/// blocks its calling thread forever once the tap installs successfully, so
/// it can't share a run loop with anything else — installing it on the real
/// main thread would block `NSApplication::run()` forever right at this
/// call, before the Cocoa event loop ever started pumping.
///
/// Returns a channel of every `KeyCombo` press found in `active_bindings`
/// at the moment it's pressed. Matched presses are consumed (dropped) so
/// they don't leak into the focused app as literal keystrokes; anything not
/// in `active_bindings` passes through untouched. The channel is a
/// `tokio::sync::mpsc::UnboundedSender`, sent to directly from this
/// dedicated thread — `send` is a plain synchronous call, so no separate
/// bridge thread is needed to get these events into the daemon's Tokio
/// runtime.
///
/// `active_bindings` is checked synchronously inside the event-tap
/// callback — a `CGEventTap` callback cannot `.await` a round-trip to the
/// daemon's single-threaded state loop to ask "is this bound?" before
/// deciding whether to consume the keystroke. This is the one place in
/// tili that shares mutable state via a `Mutex` instead of funneling
/// everything through one owning loop; `tili-daemon` re-syncs the set's
/// contents after every state change that could affect the active
/// mode/bindings (see `sync_active_combos` in its `main.rs`), keeping the
/// window where it could be briefly stale as small as possible.
pub fn spawn_hotkey_tap(
    active_bindings: Arc<Mutex<HashSet<KeyCombo>>>,
) -> UnboundedReceiver<KeyCombo> {
    let (tx, rx) = unbounded_channel();

    std::thread::spawn(move || {
        let mut warned = false;
        loop {
            let active_bindings = active_bindings.clone();
            let tx = tx.clone();
            // `with_enabled` only ever returns on failure — on success it
            // calls `CFRunLoop::run_current`, which blocks this thread
            // forever pumping events. So this loop only re-attempts after
            // an install failure; there's no "it succeeded" moment to log.
            let result = CGEventTap::with_enabled(
                CGEventTapLocation::Session,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                vec![CGEventType::KeyDown],
                move |_proxy, _event_type, event| {
                    let flags = event.get_flags();
                    let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                    let combo = KeyCombo {
                        cmd: flags.contains(CGEventFlags::CGEventFlagCommand),
                        alt: flags.contains(CGEventFlags::CGEventFlagAlternate),
                        ctrl: flags.contains(CGEventFlags::CGEventFlagControl),
                        shift: flags.contains(CGEventFlags::CGEventFlagShift),
                        keycode: keycode as u16,
                    };

                    let is_bound = active_bindings
                        .lock()
                        .map(|set| set.contains(&combo))
                        .unwrap_or(false);

                    if is_bound {
                        let _ = tx.send(combo);
                        CallbackResult::Drop
                    } else {
                        CallbackResult::Keep
                    }
                },
                CFRunLoop::run_current,
            );

            if result.is_err() && !warned {
                warned = true;
                if has_input_monitoring_permission() {
                    eprintln!(
                        "tili-ax: failed to install the hotkey event tap even though Input \
                         Monitoring looks granted — check Accessibility permission in System \
                         Settings > Privacy & Security > Accessibility for the running \
                         tili-daemon binary. Will keep retrying."
                    );
                } else {
                    eprintln!(
                        "tili-ax: failed to install the hotkey event tap — Input Monitoring \
                         permission is missing. Grant it in System Settings > Privacy & \
                         Security > Input Monitoring for the running tili-daemon binary; \
                         hotkeys will start working automatically once granted, no restart \
                         needed."
                    );
                }
            }

            std::thread::sleep(HOTKEY_TAP_RETRY_INTERVAL);
        }
    });

    rx
}

/// Parses a KDL keybinding key string (e.g. `"alt-shift-h"`) into a
/// `KeyCombo`. Modifier tokens may appear in any order; the trailing token
/// must be a recognized base key. Returns `None` for anything unrecognized
/// (an unknown key name, or a string with no base key) — callers should
/// treat that as "this binding can't be installed" and skip it rather than
/// fail the whole config.
pub fn parse_key_combo(s: &str) -> Option<KeyCombo> {
    let mut cmd = false;
    let mut alt = false;
    let mut ctrl = false;
    let mut shift = false;
    let mut keycode = None;

    for token in s.split('-') {
        match token {
            "cmd" | "command" => cmd = true,
            "alt" | "option" => alt = true,
            "ctrl" | "control" => ctrl = true,
            "shift" => shift = true,
            key => keycode = Some(key_code_for_name(key)?),
        }
    }

    keycode.map(|keycode| KeyCombo {
        cmd,
        alt,
        ctrl,
        shift,
        keycode,
    })
}

#[rustfmt::skip]
fn key_code_for_name(name: &str) -> Option<u16> {
    Some(match name {
        "a" => KeyCode::ANSI_A, "b" => KeyCode::ANSI_B, "c" => KeyCode::ANSI_C,
        "d" => KeyCode::ANSI_D, "e" => KeyCode::ANSI_E, "f" => KeyCode::ANSI_F,
        "g" => KeyCode::ANSI_G, "h" => KeyCode::ANSI_H, "i" => KeyCode::ANSI_I,
        "j" => KeyCode::ANSI_J, "k" => KeyCode::ANSI_K, "l" => KeyCode::ANSI_L,
        "m" => KeyCode::ANSI_M, "n" => KeyCode::ANSI_N, "o" => KeyCode::ANSI_O,
        "p" => KeyCode::ANSI_P, "q" => KeyCode::ANSI_Q, "r" => KeyCode::ANSI_R,
        "s" => KeyCode::ANSI_S, "t" => KeyCode::ANSI_T, "u" => KeyCode::ANSI_U,
        "v" => KeyCode::ANSI_V, "w" => KeyCode::ANSI_W, "x" => KeyCode::ANSI_X,
        "y" => KeyCode::ANSI_Y, "z" => KeyCode::ANSI_Z,
        "0" => KeyCode::ANSI_0, "1" => KeyCode::ANSI_1, "2" => KeyCode::ANSI_2,
        "3" => KeyCode::ANSI_3, "4" => KeyCode::ANSI_4, "5" => KeyCode::ANSI_5,
        "6" => KeyCode::ANSI_6, "7" => KeyCode::ANSI_7, "8" => KeyCode::ANSI_8,
        "9" => KeyCode::ANSI_9,
        "slash" => KeyCode::ANSI_SLASH,
        "semicolon" => KeyCode::ANSI_SEMICOLON,
        "minus" => KeyCode::ANSI_MINUS,
        "equal" => KeyCode::ANSI_EQUAL,
        "comma" => KeyCode::ANSI_COMMA,
        "period" => KeyCode::ANSI_PERIOD,
        "quote" => KeyCode::ANSI_QUOTE,
        "grave" => KeyCode::ANSI_GRAVE,
        "leftbracket" => KeyCode::ANSI_LEFT_BRACKET,
        "rightbracket" => KeyCode::ANSI_RIGHT_BRACKET,
        "backslash" => KeyCode::ANSI_BACKSLASH,
        "escape" => KeyCode::ESCAPE,
        "enter" | "return" => KeyCode::RETURN,
        "space" => KeyCode::SPACE,
        "tab" => KeyCode::TAB,
        "delete" => KeyCode::DELETE,
        "left" => KeyCode::LEFT_ARROW,
        "right" => KeyCode::RIGHT_ARROW,
        "up" => KeyCode::UP_ARROW,
        "down" => KeyCode::DOWN_ARROW,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_modifier_combo() {
        let combo = parse_key_combo("alt-h").unwrap();
        assert!(combo.alt);
        assert!(!combo.shift && !combo.cmd && !combo.ctrl);
        assert_eq!(combo.keycode, KeyCode::ANSI_H);
    }

    #[test]
    fn modifier_order_does_not_matter() {
        assert_eq!(
            parse_key_combo("alt-shift-h"),
            parse_key_combo("shift-alt-h")
        );
    }

    #[test]
    fn parses_special_keys() {
        assert_eq!(parse_key_combo("escape").unwrap().keycode, KeyCode::ESCAPE);
        assert_eq!(
            parse_key_combo("alt-slash").unwrap().keycode,
            KeyCode::ANSI_SLASH
        );
    }

    #[test]
    fn unknown_key_name_returns_none() {
        assert_eq!(parse_key_combo("alt-notakey"), None);
        assert_eq!(parse_key_combo(""), None);
    }
}
