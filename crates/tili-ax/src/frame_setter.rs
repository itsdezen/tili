use std::collections::HashMap;
use std::time::{Duration, Instant};

use tili_tree::{Rect, WindowId};

use crate::window::{AxWindow, frame_matches};

/// The seam that lets v2 slot in animated window movement without touching
/// any layout or tree-mutation code above this trait. v1 only implements
/// `InstantFrameSetter`; `TweenedFrameSetter` is v2.
///
/// Takes `&mut AxWindow` because setting a frame also updates the window's
/// cached frame to match (see `AxWindow::set_frame`) — callers like
/// `WmState::list_windows` read that cache rather than re-querying AX on
/// every request, so it needs to stay truthful immediately after a write,
/// including for off-screen "parked" windows (M4).
pub trait WindowFrameSetter {
    fn set_frame(&mut self, window: &mut AxWindow, target: Rect);

    /// Advances any in-flight animations by one tick, writing intermediate
    /// frames directly on whichever `AxWindow`s are still mid-tween. Called
    /// from `main.rs`'s dedicated animation timer, gated by
    /// `has_active_animations` so it only ever runs while something is
    /// actually mid-tween. No-op default so `InstantFrameSetter` needs no
    /// changes.
    fn tick(&mut self, _windows: &mut HashMap<WindowId, AxWindow>) {}

    /// Whether `id` currently has an animation in flight. Callers that
    /// compare a window's live AX frame against a previously-cached one to
    /// detect a user's own drag (`WmState::maybe_capture_manual_geometry`)
    /// must skip that comparison while true, since every animation step is
    /// a real, tili-initiated frame change, not a user drag. No-op default
    /// (`false`) so `InstantFrameSetter` needs no changes.
    fn is_animating(&self, _id: WindowId) -> bool {
        false
    }

    /// Whether *any* window currently has an animation in flight — gates
    /// `main.rs`'s dedicated animation tick (`if state.is_animating_anything()`
    /// on the `tokio::select!` branch), so that timer is never even polled,
    /// let alone fires, while nothing is animating. No-op default
    /// (`false`) so `InstantFrameSetter` needs no changes.
    fn has_active_animations(&self) -> bool {
        false
    }

    /// If `window` has an animation in flight, instantly writes its true
    /// target (bypassing interpolation) and stops tracking it; otherwise a
    /// no-op. Every write that bypasses this trait entirely and calls
    /// `AxWindow`'s methods directly (`park`, `place_floating_window`'s
    /// centered branch) must call this first — otherwise a stale tween
    /// could resume on a later tick and visibly drag the window away from
    /// wherever that direct write just put it. No-op default so
    /// `InstantFrameSetter` needs no changes.
    fn finish(&mut self, _window: &mut AxWindow) {}

    /// While `suppressed` is true, `set_frame` writes instantly instead of
    /// animating, same as `InstantFrameSetter` — for a caller that needs a
    /// specific sequence of writes to skip animation regardless of
    /// `Settings::animate` (e.g. revealing a workspace's previously-parked
    /// windows: there's no meaningful start point to ease from, since the
    /// parked position was never meant to be seen). No-op default so
    /// `InstantFrameSetter` needs no changes — every write it makes is
    /// already instant.
    fn set_suppressed(&mut self, _suppressed: bool) {}
}

pub struct InstantFrameSetter;

impl WindowFrameSetter for InstantFrameSetter {
    fn set_frame(&mut self, window: &mut AxWindow, target: Rect) {
        window.set_frame(target);
    }
}

/// One window's in-flight animation: the frame it started from, the frame
/// it's heading to, and when it started. `TweenedFrameSetter::tick`
/// re-derives the current interpolated frame from these on every call
/// rather than storing a running position, so restarting a tween
/// (`set_frame` called again before the previous one finished) is just
/// replacing this struct — never fighting stale progress toward an
/// already-superseded target.
struct Tween {
    start: Rect,
    target: Rect,
    started: Instant,
}

/// Animates window-frame writes with a short ease-out tween instead of
/// jumping straight to the target — the "v2" `WindowFrameSetter`'s own doc
/// comment refers to. All animation state lives here rather than on
/// `WmState`, matching the project's principle that `WindowFrameSetter` is
/// the only thing allowed to know how a window's frame actually gets set.
pub struct TweenedFrameSetter {
    active: HashMap<WindowId, Tween>,
    duration: Duration,
    suppressed: bool,
}

impl TweenedFrameSetter {
    pub fn new(duration: Duration) -> Self {
        Self {
            active: HashMap::new(),
            duration,
            suppressed: false,
        }
    }
}

impl WindowFrameSetter for TweenedFrameSetter {
    fn set_frame(&mut self, window: &mut AxWindow, target: Rect) {
        let id = window.id();

        if self.suppressed {
            self.active.remove(&id);
            window.set_frame(target);
            return;
        }

        // Every tween step is a *real* write, so it fires a real
        // `AXWindowMoved`/`AXWindowResized` notification — unlike
        // `InstantFrameSetter`'s single write, which the no-op guard
        // silences on any immediate re-trigger. That notification reaches
        // `apply_windows_changed`, which unconditionally relays out again,
        // calling `set_frame` with the *same* target on every tick for as
        // long as the animation runs. If that re-triggered call restarted
        // the tween's clock, `started` would keep resetting to "now" and
        // `tick` would never see enough elapsed time to reach `duration` —
        // the animation would stall just past its start and never
        // converge. So a target matching the tween already in flight is a
        // no-op on the tween itself: leave its clock untouched.
        if let Some(existing) = self.active.get(&id)
            && frame_matches(existing.target, target)
        {
            return;
        }

        // `window.frame()` is the cached, most-recently-actually-written
        // frame — for a window already mid-tween, `tick` keeps that in
        // sync with the interpolated frame it just wrote, so using it here
        // naturally restarts a *genuinely new* target from wherever the
        // window visually is right now, not from a stale original start
        // point.
        let start = window.frame();
        if frame_matches(start, target) {
            window.set_frame(target);
            self.active.remove(&id);
            return;
        }
        self.active.insert(
            id,
            Tween {
                start,
                target,
                started: Instant::now(),
            },
        );
    }

    fn tick(&mut self, windows: &mut HashMap<WindowId, AxWindow>) {
        self.active.retain(|&id, tween| {
            let Some(window) = windows.get_mut(&id) else {
                // Closed mid-animation — nothing left to write to.
                return false;
            };
            let t = tween.started.elapsed().as_secs_f64() / self.duration.as_secs_f64();
            if t >= 1.0 {
                window.set_frame(tween.target);
                return false;
            }
            window.set_frame(interpolate(tween.start, tween.target, ease_out(t)));
            true
        });
    }

    fn is_animating(&self, id: WindowId) -> bool {
        self.active.contains_key(&id)
    }

    fn has_active_animations(&self) -> bool {
        !self.active.is_empty()
    }

    fn finish(&mut self, window: &mut AxWindow) {
        if let Some(tween) = self.active.remove(&window.id()) {
            window.set_frame(tween.target);
        }
    }

    fn set_suppressed(&mut self, suppressed: bool) {
        self.suppressed = suppressed;
    }
}

/// Ease-out quad: fast start, settling gently into `target` rather than
/// stopping abruptly — reads as a snap, not a linear slide.
fn ease_out(t: f64) -> f64 {
    1.0 - (1.0 - t) * (1.0 - t)
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

fn interpolate(start: Rect, target: Rect, t: f64) -> Rect {
    Rect {
        x: lerp(start.x, target.x, t),
        y: lerp(start.y, target.y, t),
        width: lerp(start.width, target.width, t),
        height: lerp(start.height, target.height, t),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ease_out_bounds_and_shape() {
        assert_eq!(ease_out(0.0), 0.0);
        assert_eq!(ease_out(1.0), 1.0);
        // Fast start: ahead of a linear ramp at the midpoint.
        assert!(ease_out(0.5) > 0.5);
    }

    #[test]
    fn interpolate_bounds_and_midpoint() {
        let start = Rect {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        };
        let target = Rect {
            x: 100.0,
            y: 200.0,
            width: 300.0,
            height: 400.0,
        };
        assert_eq!(interpolate(start, target, 0.0), start);
        assert_eq!(interpolate(start, target, 1.0), target);
        assert_eq!(
            interpolate(start, target, 0.5),
            Rect {
                x: 50.0,
                y: 100.0,
                width: 200.0,
                height: 250.0,
            }
        );
    }
}
