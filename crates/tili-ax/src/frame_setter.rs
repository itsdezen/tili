use tili_tree::Rect;

use crate::window::AxWindow;

/// The seam that lets v2 slot in animated window movement without touching
/// any layout or tree-mutation code above this trait. v1 only implements
/// `InstantFrameSetter`.
///
/// Takes `&mut AxWindow` because setting a frame also updates the window's
/// cached frame to match (see `AxWindow::set_frame`) — callers like
/// `WmState::list_windows` read that cache rather than re-querying AX on
/// every request, so it needs to stay truthful immediately after a write,
/// including for off-screen "parked" windows (M4).
pub trait WindowFrameSetter {
    fn set_frame(&self, window: &mut AxWindow, target: Rect);
}

pub struct InstantFrameSetter;

impl WindowFrameSetter for InstantFrameSetter {
    fn set_frame(&self, window: &mut AxWindow, target: Rect) {
        window.set_frame(target);
    }
}
