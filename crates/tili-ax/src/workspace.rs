use std::ptr::NonNull;
use std::sync::mpsc::Sender;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication, NSWorkspace};
use objc2_foundation::{NSNotification, NSOperationQueue};

unsafe extern "C" {
    fn CFRunLoopGetCurrent() -> *mut std::ffi::c_void;
    fn CFRunLoopRun();
}

/// A process launching or quitting, as reported by `NSWorkspace`.
#[derive(Debug, Clone)]
pub enum AppEvent {
    Launched { pid: i32, bundle_id: Option<String> },
    Terminated { pid: i32 },
}

/// Spawns a dedicated OS thread that registers for
/// `NSWorkspaceDidLaunchApplicationNotification`/`DidTerminateApplication`
/// and pumps a `CFRunLoop` on that thread for the lifetime of the process.
///
/// This mirrors exactly the pattern `axuielement`'s own `AXNotificationStream`
/// uses for AX notifications: Cocoa/AX notification delivery for a
/// non-`NSApplication` process needs *some* thread running an active
/// `CFRunLoop` to receive the underlying system messages, regardless of
/// which `NSOperationQueue` the resulting block is dispatched onto — so a
/// dedicated thread is created and immediately parked in `CFRunLoopRun()`
/// after registering the observer.
pub fn spawn_workspace_watcher(tx: Sender<AppEvent>) {
    std::thread::spawn(move || {
        // SAFETY: `sharedWorkspace`/`notificationCenter` are safe to call
        // off the main thread (objc2-app-kit does not gate them behind
        // `MainThreadMarker`); the block below only touches `Send` data
        // (`Sender<AppEvent>`, primitives) and is itself `Send`.
        unsafe {
            let workspace = NSWorkspace::sharedWorkspace();
            let center = workspace.notificationCenter();

            let launched_tx = tx.clone();
            let launched_block = RcBlock::new(move |note: NonNull<NSNotification>| {
                if let Some((pid, bundle_id)) = running_app_from_notification(note) {
                    let _ = launched_tx.send(AppEvent::Launched { pid, bundle_id });
                }
            });
            center.addObserverForName_object_queue_usingBlock(
                Some(objc2_app_kit::NSWorkspaceDidLaunchApplicationNotification),
                None,
                None::<&NSOperationQueue>,
                &launched_block,
            );

            let terminated_tx = tx.clone();
            let terminated_block = RcBlock::new(move |note: NonNull<NSNotification>| {
                if let Some((pid, _)) = running_app_from_notification(note) {
                    let _ = terminated_tx.send(AppEvent::Terminated { pid });
                }
            });
            center.addObserverForName_object_queue_usingBlock(
                Some(objc2_app_kit::NSWorkspaceDidTerminateApplicationNotification),
                None,
                None::<&NSOperationQueue>,
                &terminated_block,
            );

            CFRunLoopGetCurrent();
            CFRunLoopRun();
        }
    });
}

/// Resolves a running process's bundle identifier (e.g.
/// `"com.apple.finder"`) from its pid — used to match a newly-created
/// window's owning app against `floating-rules` (M8). `None` for
/// processes with no bundle id (most background/helper processes; regular
/// GUI apps always have one).
pub fn bundle_id_for_pid(pid: i32) -> Option<String> {
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
    app.bundleIdentifier().map(|s| s.to_string())
}

/// Brings a process's application frontmost, activating it — best-effort,
/// same as every other AX/`NSRunningApplication` write in this crate.
/// `AXUIElement`'s own `kAXRaiseAction`/`kAXFocusedAttribute` (used by
/// `AxWindow::focus`) only raises a window within its own app's window
/// stack; if a *different* app is currently frontmost, the target window
/// never actually becomes interactive/visibly focused without also
/// activating its owning app — this is what makes cross-app `focus`
/// switches take visible effect, not just same-app ones (which already
/// looked like they worked, since the frontmost app didn't need to change).
pub fn activate_app(pid: i32) {
    if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) {
        app.activateWithOptions(NSApplicationActivationOptions::empty());
    }
}

/// Whether a process's app is currently hidden (Cmd-H / "Hide app-name"),
/// via `NSRunningApplication.isHidden`. Called once per pid per refresh,
/// same cost profile as `bundle_id_for_pid` — used to classify a process's
/// windows as `PlacementKind::HiddenApplication` rather than trying to
/// force them back on screen.
pub fn is_app_hidden(pid: i32) -> bool {
    NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        .is_some_and(|app| app.isHidden())
}

/// Extracts the launched/terminated app's pid and bundle id from a
/// `NSWorkspaceDidLaunchApplicationNotification`/`DidTerminateApplication`'s
/// `userInfo[NSWorkspaceApplicationKey]`.
///
/// SAFETY: `note` is a valid `NSNotification` for the lifetime of the call,
/// as guaranteed by the Cocoa notification-center callback contract.
unsafe fn running_app_from_notification(
    note: NonNull<NSNotification>,
) -> Option<(i32, Option<String>)> {
    let note = unsafe { note.as_ref() };
    let user_info = note.userInfo()?;
    let key: &AnyObject = unsafe { objc2_app_kit::NSWorkspaceApplicationKey };
    let app_obj = user_info.objectForKey(key)?;
    let app: Retained<NSRunningApplication> = app_obj.downcast().ok()?;
    let pid = app.processIdentifier();
    let bundle_id = app.bundleIdentifier().map(|s| s.to_string());
    Some((pid, bundle_id))
}
