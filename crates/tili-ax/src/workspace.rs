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
use objc2_foundation::{
    NSDistributedNotificationCenter, NSNotification, NSOperationQueue, NSString,
};

/// A process launching, quitting, or becoming the system-wide frontmost
/// application, the system waking from sleep, or the screen
/// locking/unlocking — the first four as reported by `NSWorkspace`,
/// `ScreenLocked`/`ScreenUnlocked` as reported by
/// `NSDistributedNotificationCenter` instead (see `register_on_main`'s doc
/// comment on why that's a distinct notification center, not
/// `NSWorkspace`'s).
#[derive(Debug, Clone)]
pub enum AppEvent {
    Launched {
        pid: i32,
        bundle_id: Option<String>,
    },
    Terminated {
        pid: i32,
    },
    Activated {
        pid: i32,
    },
    SystemDidWake,
    /// `com.apple.screenIsLocked`. Confirmed on real hardware that this is
    /// *not* the same event as `SystemDidWake`/`NSWorkspaceDidWakeNotification`
    /// — locking the screen (Control+Cmd+Q, the menu-bar lock icon, or an
    /// idle timeout) and waiting for the display to blank, which is how
    /// this project's own testing showed the user actually "puts the
    /// machine to sleep" day to day, never fires the `NSWorkspace` wake
    /// notification at all, since the machine itself never actually
    /// suspends. See `tili-daemon`'s `WmState::note_screen_locked` for what
    /// reacting to this does.
    ScreenLocked,
    /// `com.apple.screenIsUnlocked` — see `ScreenLocked`'s doc comment and
    /// `WmState::note_screen_unlocked`.
    ScreenUnlocked,
}

/// Registers `NSWorkspaceDidLaunchApplicationNotification`/
/// `DidTerminateApplication`/`DidActivateApplication`/`DidWakeNotification`
/// (via `NSWorkspace`'s own notification center) and
/// `com.apple.screenIsLocked`/`screenIsUnlocked` (via
/// `NSDistributedNotificationCenter`) on the real process main thread, with
/// `queue: .main` delivery, and returns immediately (no thread spawned, no
/// `CFRunLoop` pumped here) — `NSApplication::run()`, called by the caller
/// afterward, is what pumps the main run loop that delivers these blocks.
/// Must be called after a real `NSApplication` instance exists and before
/// `run()` starts, on the real main thread (the `MainThreadMarker`
/// parameter documents that requirement; the `objc2-app-kit`/
/// `objc2-foundation` calls themselves aren't gated behind it).
///
/// Main-thread registration with `queue: .main` is load-bearing, not
/// incidental: confirmed on real hardware that `DidLaunchApplication`/
/// `DidWakeNotification` are not reliably delivered to a process with no
/// `NSApplication` pumping its main run loop (only `DidTerminateApplication`
/// was, evidently via some other delivery path) — a bare `CFRunLoopRun()`
/// on an arbitrary background thread, with `queue: nil`, isn't sufficient
/// for these specifically, unlike AX notifications elsewhere in this crate.
/// `DidActivateApplication` is registered here on the same premise, also
/// separately confirmed reliably delivered on real hardware, replacing
/// `tili-ax/src/watch.rs`'s periodic poll of `frontmost_app_pid()`
/// for `WmEvent::FrontmostAppChanged` — see that event's doc comment.
///
/// The screen-lock pair uses `NSDistributedNotificationCenter::defaultCenter()`
/// instead of `NSWorkspace`'s center — `com.apple.screenIsLocked`/
/// `screenIsUnlocked` are undocumented, system-wide distributed
/// notifications (posted by `loginwindow`, not `NSWorkspace`), the
/// long-standing community-known way to observe the screen lock/unlock the
/// user actually triggers day to day (Control+Cmd+Q, the menu-bar lock
/// icon, or an idle timeout) — confirmed on real hardware that this is a
/// *materially different* event from `NSWorkspaceDidWakeNotification`:
/// locking and waiting for the display to blank never fires the
/// `NSWorkspace` sleep/wake pair at all, since the machine itself never
/// actually suspends. `NSDistributedNotificationCenter` is a subclass of
/// `NSNotificationCenter` (its own `addObserverForName:object:queue:usingBlock:`
/// is inherited, not reimplemented), so the same block/`queue: .main`
/// registration shape applies unchanged.
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

        let activated_tx = tx.clone();
        let activated_block = RcBlock::new(move |note: NonNull<NSNotification>| {
            if let Some((pid, _)) = running_app_from_notification(note) {
                let _ = activated_tx.send(AppEvent::Activated { pid });
            }
        });
        center.addObserverForName_object_queue_usingBlock(
            Some(objc2_app_kit::NSWorkspaceDidActivateApplicationNotification),
            None,
            Some(&main_queue),
            &activated_block,
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

        let distributed_center = NSDistributedNotificationCenter::defaultCenter();
        let screen_locked_name = NSString::from_str("com.apple.screenIsLocked");
        let locked_tx = tx.clone();
        let locked_block = RcBlock::new(move |_note: NonNull<NSNotification>| {
            let _ = locked_tx.send(AppEvent::ScreenLocked);
        });
        distributed_center.addObserverForName_object_queue_usingBlock(
            Some(&screen_locked_name),
            None,
            Some(&main_queue),
            &locked_block,
        );

        let screen_unlocked_name = NSString::from_str("com.apple.screenIsUnlocked");
        let unlocked_tx = tx.clone();
        let unlocked_block = RcBlock::new(move |_note: NonNull<NSNotification>| {
            let _ = unlocked_tx.send(AppEvent::ScreenUnlocked);
        });
        distributed_center.addObserverForName_object_queue_usingBlock(
            Some(&screen_unlocked_name),
            None,
            Some(&main_queue),
            &unlocked_block,
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
/// notification. `tili-ax/src/watch.rs`'s periodic tick used to poll this
/// for `WmEvent::FrontmostAppChanged` before `register_on_main` registered
/// `NSWorkspaceDidActivateApplicationNotification` (confirmed on real
/// hardware to never fire for `tili-daemon` prior to `main.rs` giving it a
/// real `NSApplication` instance and main-thread registration) — that tick
/// now reacts to the push notification instead. `frontmost_app_pid` itself
/// stays: `WmState::sync_focus_from_pid` and `reveal_current_frontmost`
/// both still call it directly, as a synchronous "what's frontmost right
/// now" read at a specific decision point, not a periodic poll.
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
