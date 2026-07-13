use std::cell::Cell;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use core_foundation::runloop::CFRunLoop;
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    CallbackResult,
};
use core_graphics::geometry::CGPoint;

/// How often the mouse-moved watcher is allowed to report a position —
/// bounds the channel/wakeup volume during active mouse movement (which is
/// common, unlike the rest of tili's event sources) to something trivial
/// rather than one message per pixel of travel. Coarse enough that
/// focus-follows-monitor still feels instant to a human, fine enough that
/// it never meaningfully lags behind the cursor.
const THROTTLE: Duration = Duration::from_millis(80);

/// Moves the system cursor without generating a synthetic click — used for
/// mouse-follows-focus. Best-effort: if this fails (e.g. permission
/// revoked mid-session) the cursor just doesn't move, nothing else breaks.
pub fn warp_cursor_to(x: f64, y: f64) {
    let _ = CGDisplay::warp_mouse_cursor_position(CGPoint::new(x, y));
}

/// Spawns a dedicated thread with a `CGEventTap` (`ListenOnly` — this only
/// observes movement, it never consumes or alters it) reporting the
/// cursor's global position, throttled to `THROTTLE`, for
/// focus-follows-monitor. Same dedicated-`CFRunLoop`-thread pattern as
/// every other watcher in this crate.
pub fn spawn_mouse_watcher() -> Receiver<(f64, f64)> {
    let (tx, rx) = channel();

    std::thread::spawn(move || {
        let last_sent: Cell<Option<Instant>> = Cell::new(None);

        let result = CGEventTap::with_enabled(
            CGEventTapLocation::Session,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::ListenOnly,
            vec![CGEventType::MouseMoved],
            move |_proxy, _event_type, event| {
                let due = last_sent.get().is_none_or(|t| t.elapsed() >= THROTTLE);
                if due {
                    let point = event.location();
                    let _ = tx.send((point.x, point.y));
                    last_sent.set(Some(Instant::now()));
                }
                CallbackResult::Keep
            },
            CFRunLoop::run_current,
        );

        if result.is_err() {
            eprintln!(
                "tili-ax: failed to install the mouse-move event tap — \
                 focus-follows-monitor will stay inactive"
            );
        }
    });

    rx
}
