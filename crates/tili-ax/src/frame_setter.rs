use tili_tree::Rect;

use crate::window::AxWindow;

/// The seam that lets v2 slot in animated window movement without touching
/// any layout or tree-mutation code above this trait. v1 only implements
/// `InstantFrameSetter`.
pub trait WindowFrameSetter {
    fn set_frame(&self, window: &AxWindow, target: Rect);
}

pub struct InstantFrameSetter;

impl WindowFrameSetter for InstantFrameSetter {
    fn set_frame(&self, window: &AxWindow, target: Rect) {
        window.set_frame(target);
    }
}
