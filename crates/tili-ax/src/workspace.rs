use std::ptr::NonNull;
use std::sync::mpsc::Receiver;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSApplicationActivationOptions, NSApplicationActivationPolicy, NSRunningApplication,
    NSWorkspace,
};
use objc2_foundation::{NSNotification, NSOperationQueue};

/// A process launching or quitting, or the system waking from sleep, as
/// reported by `NSWorkspace`.
#[derive(Debug, Clone)]
pub enum AppEvent {
    Launched { pid: i32, bundle_id: Option<String> },
    Terminated { pid: i32 },
    SystemDidWake,
}

/// Registers `NSWorkspaceDidLaunchApplicationNotification`/
/// `DidTerminateApplication`/`DidWakeNotification` on the real process main
/// thread, with `queue: .main` delivery, and returns immediately (no thread
/// spawned, no `CFRunLoop` pumped here) — `NSApplication::run()`, called by
/// the caller afterward, is what pumps the main run loop that delivers
/// these blocks. Must be called after a real `NSApplication` instance
/// exists and before `run()` starts, on the real main thread (the
/// `MainThreadMarker` parameter documents that requirement; the
/// `objc2-app-kit` calls themselves aren't gated behind it).
///
/// Main-thread registration with `queue: .main` is load-bearing, not
/// incidental: confirmed on real hardware that `DidLaunchApplication`/
/// `DidWakeNotification` are not reliably delivered to a process with no
/// `NSApplication` pumping its main run loop (only `DidTerminateApplication`
/// was, evidently via some other delivery path) — a bare `CFRunLoopRun()`
/// on an arbitrary background thread, with `queue: nil`, isn't sufficient
/// for these two specifically, unlike AX notifications elsewhere in this
/// crate.
pub fn register_on_main(_mtm: MainThreadMarker) -> Receiver<AppEvent> {
    let (tx, rx) = std::sync::mpsc::channel();
    // SAFETY: `sharedWorkspace`/`notificationCenter` are safe to call from
    // any thread (objc2-app-kit does not gate them behind
    // `MainThreadMarker`); the blocks below only touch `Send` data
    // (`Sender<AppEvent>`, primitives) and are themselves `Send`.
    unsafe {
        let workspace = NSWorkspace::sharedWorkspace();
        let center = workspace.notificationCenter();
        let main_queue = NSOperationQueue::mainQueue();

        let launched_tx = tx.clone();
        let launched_block = RcBlock::new(move |note: NonNull<NSNotification>| {
            if let Some((pid, bundle_id)) = running_app_from_notification(note) {
                let _ = launched_tx.send(AppEvent::Launched { pid, bundle_id });
            }
        });
        center.addObserverForName_object_queue_usingBlock(
            Some(objc2_app_kit::NSWorkspaceDidLaunchApplicationNotification),
            None,
            Some(&main_queue),
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
            Some(&main_queue),
            &terminated_block,
        );

        let wake_tx = tx.clone();
        let wake_block = RcBlock::new(move |_note: NonNull<NSNotification>| {
            let _ = wake_tx.send(AppEvent::SystemDidWake);
        });
        center.addObserverForName_object_queue_usingBlock(
            Some(objc2_app_kit::NSWorkspaceDidWakeNotification),
            None,
            Some(&main_queue),
            &wake_block,
        );
    }
    rx
}

/// Every currently running "regular" (Dock-visible) application's pid,
/// regardless of whether it has any on-screen window right now — unlike
/// `enumerate::onscreen_owner_pids`, which only sees apps with a window
/// open at this exact moment. An app that's still running with zero
/// windows (e.g. the user closed all of its windows without quitting it)
/// never fires `NSWorkspaceDidLaunchApplicationNotification` again when it
/// later opens a *new* window — its process never actually launches afresh
/// — so it needs a `watch_app` subscription set up proactively, before
/// that happens, or its first new window has nothing watching for it.
pub fn all_regular_app_pids() -> Vec<i32> {
    let workspace = NSWorkspace::sharedWorkspace();
    workspace
        .runningApplications()
        .to_vec()
        .into_iter()
        .filter(|app| app.activationPolicy() == NSApplicationActivationPolicy::Regular)
        .map(|app| app.processIdentifier())
        .collect()
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

/// The pid of the currently frontmost application, via the system-wide
/// Accessibility element (`AXUIElementCreateSystemWide`'s
/// `AXFocusedApplication` attribute) — a direct, synchronous query, not a
/// notification. Used instead of `NSWorkspaceDidActivateApplicationNotification`
/// — confirmed on real hardware to never fire for `tili-daemon` prior to
/// `main.rs` giving it a real `NSApplication` instance and main-thread
/// `register_on_main` registration (see that function's doc comment); even
/// `DidLaunchApplication`/`DidWakeNotification` were confirmed silently
/// undelivered without that context, and only `DidTerminateApplication`
/// worked reliably beforehand. `frontmost_app_pid` stays a polled query
/// rather than switching to `DidActivateApplication` now that a real
/// `NSApplication` exists, since it's cheap and already proven — `watch.rs`
/// polls this on its existing cheap resync tick.
pub fn frontmost_app_pid() -> Option<i32> {
    axuielement::system_wide()?
        .focused_application()
        .ok()
        .flatten()?
        .pid()
        .ok()
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

/// Whether a process's `NSApplication.ActivationPolicy` is `.regular` (Dock
/// icon + Cmd-Tab visible) as opposed to `.accessory`/`.prohibited` — used
/// alongside close-button existence in `window.rs::classify_window_kind` to
/// filter out transient system-UI chrome (context menus, thumbnail
/// previews, authorization prompts). A process defaults to "not regular"
/// (`false`) if it can't be resolved at all, matching the conservative
/// default `all_regular_app_pids`'s own filter already takes.
pub fn is_regular_app(pid: i32) -> bool {
    NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        .is_some_and(|app| app.activationPolicy() == NSApplicationActivationPolicy::Regular)
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
