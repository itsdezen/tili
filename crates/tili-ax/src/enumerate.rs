use std::collections::BTreeSet;

use axuielement::AXUIElement;
use axuielement::ffi::kAXWindowsAttribute;
use core_foundation::base::TCFType;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_graphics::window::{
    copy_window_info, kCGNullWindowID, kCGWindowListExcludeDesktopElements,
    kCGWindowListOptionOnScreenOnly, kCGWindowOwnerPID,
};

use crate::window::AxWindow;

/// Distinct owner PIDs of currently on-screen windows, found via the public
/// `CGWindowListCopyWindowInfo` API. This needs no permission beyond what
/// tili already requires (Accessibility) — window ownership/bounds are
/// available without Screen Recording permission; only window *names* read
/// through this API would need that, so we deliberately don't read
/// `kCGWindowName` here and get titles through the AX API instead.
fn onscreen_owner_pids() -> BTreeSet<i32> {
    let mut pids = BTreeSet::new();
    let Some(windows) = copy_window_info(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
        kCGNullWindowID,
    ) else {
        return pids;
    };

    for i in 0..windows.len() {
        let Some(entry) = windows.get(i) else {
            continue;
        };
        let entry_ptr: *const std::ffi::c_void = *entry;
        // SAFETY: entries in the array returned by CGWindowListCopyWindowInfo
        // are CFDictionary instances per Apple's documented contract.
        let dict = unsafe {
            CFDictionary::<core_foundation::string::CFString, core_foundation::base::CFType>::wrap_under_get_rule(
                entry_ptr.cast(),
            )
        };
        if let Some(owner_pid) = dict.find(unsafe { kCGWindowOwnerPID })
            && let Some(pid) = owner_pid.downcast::<CFNumber>().and_then(|n| n.to_i32())
        {
            pids.insert(pid);
        }
    }
    pids
}

/// Enumerates real, currently-open windows: finds owning processes via the
/// public CoreGraphics window list, then reads each process's `AXWindows`
/// through the public Accessibility API. Windows the private
/// `_AXUIElementGetWindow` call can't resolve an id for are skipped.
pub fn list_windows() -> Vec<AxWindow> {
    let mut windows = Vec::new();

    for pid in onscreen_owner_pids() {
        let Some(app) = AXUIElement::from_pid(pid) else {
            continue;
        };
        let Ok(ax_windows) = app.element_array_attribute(kAXWindowsAttribute) else {
            continue;
        };
        for element in ax_windows {
            if let Some(window) = AxWindow::from_element(element, pid) {
                windows.push(window);
            }
        }
    }

    windows
}
