use objc2::MainThreadMarker;
use tray_icon::menu::{Menu, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

use crate::badge;

/// What one poll tick fetched from the daemon — `None` (rather than a
/// bare socket error) when a request fails, so `apply_snapshot` can leave
/// the previous badge/menu on screen instead of flashing a broken state
/// on a transient daemon hiccup.
pub struct Snapshot {
    /// The focused monitor's active workspace — the unambiguous "current
    /// workspace" for a single badge, unlike `WorkspaceInfo::active`,
    /// which can be true on more than one entry across monitors.
    pub current: Option<String>,
    pub workspaces: Vec<tili_ipc::WorkspaceInfo>,
}

/// Builds the status item with a menu containing only the static "Open
/// Settings"/"Quit" items (the workspace section is added by the first
/// `apply_snapshot` call once real data arrives), hidden until the first
/// successful poll confirms the daemon is actually reachable — never
/// shows a placeholder badge that might be lying about the daemon being
/// up.
pub fn build_initial(mtm: MainThreadMarker) -> TrayIcon {
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(build_menu(None, &[])))
        .build()
        .expect("build NSStatusItem");
    set_badge(&tray, mtm, "tili");
    set_visible(&tray, mtm, false);
    tray
}

/// Polls `Command::ListMonitors` (for the focused monitor's current
/// workspace) and `Command::ListWorkspaces` (the full switchable list) —
/// best-effort, `None` on any transport/decode failure.
pub fn poll_daemon() -> Option<Snapshot> {
    let monitors = match crate::ipc::send(tili_ipc::Command::ListMonitors).ok()? {
        tili_ipc::Response::OkWithPayload(v) => {
            serde_json::from_value::<Vec<tili_ipc::MonitorInfo>>(v).ok()?
        }
        _ => return None,
    };
    let workspaces = match crate::ipc::send(tili_ipc::Command::ListWorkspaces).ok()? {
        tili_ipc::Response::OkWithPayload(v) => {
            serde_json::from_value::<Vec<tili_ipc::WorkspaceInfo>>(v).ok()?
        }
        _ => return None,
    };
    let current = monitors
        .into_iter()
        .find(|m| m.focused)
        .and_then(|m| m.active_workspace);
    Some(Snapshot {
        current,
        workspaces,
    })
}

/// `(current, sorted workspace names)` — enough to detect "did anything
/// the menu displays actually change" without needing `WorkspaceInfo` to
/// implement `PartialEq` itself (`window_count`/`monitor` churn on every
/// tick and aren't shown in the menu, so they're deliberately excluded).
pub type MenuKey = (Option<String>, Vec<String>);

pub fn menu_key(snapshot: &Snapshot) -> MenuKey {
    (
        snapshot.current.clone(),
        snapshot.workspaces.iter().map(|w| w.name.clone()).collect(),
    )
}

/// What was last actually applied to the status item — carried across
/// poll ticks (see `main.rs`) so `apply_snapshot` only touches AppKit
/// state that actually needs to change. `menu_key` avoids rebuilding a
/// live `NSMenu` for no reason (see its own history: doing that
/// unconditionally on every tick caused menu clicks to fire on their own
/// with zero user interaction); `visible` avoids redundant
/// `setVisible:` calls once the daemon-reachability state settles.
#[derive(Default)]
pub struct MenuState {
    key: Option<MenuKey>,
    visible: bool,
}

/// Applies one poll result to the status item — `None` (daemon
/// unreachable, including "not running at all") hides the badge
/// entirely rather than leaving a stale one on screen, since a badge
/// showing a workspace tili isn't actually managing right now is worse
/// than showing nothing. `Some` shows it (if it was hidden) and re-titles
/// it every tick (cheap), but only replaces the `NSMenu` itself when
/// `menu_key` actually differs from what's already applied.
pub fn apply_snapshot(
    tray: &TrayIcon,
    mtm: MainThreadMarker,
    snapshot: &Option<Snapshot>,
    state: &mut MenuState,
) {
    let Some(snapshot) = snapshot else {
        if state.visible {
            set_visible(tray, mtm, false);
            state.visible = false;
        }
        return;
    };
    if !state.visible {
        set_visible(tray, mtm, true);
        state.visible = true;
    }

    let title = snapshot.current.as_deref().unwrap_or("tili");
    set_badge(tray, mtm, title);

    let key = menu_key(snapshot);
    if state.key.as_ref() == Some(&key) {
        return;
    }
    tray.set_menu(Some(Box::new(build_menu(
        snapshot.current.as_deref(),
        &snapshot.workspaces,
    ))));
    state.key = Some(key);
}

fn build_menu(current: Option<&str>, workspaces: &[tili_ipc::WorkspaceInfo]) -> Menu {
    let menu = Menu::new();
    for ws in workspaces {
        let checked = current == Some(ws.name.as_str());
        let label = if checked {
            format!("\u{2022} {}", ws.name)
        } else {
            ws.name.clone()
        };
        let item = MenuItem::with_id(
            MenuId::new(format!("workspace:{}", ws.name)),
            &label,
            true,
            None,
        );
        let _ = menu.append(&item);
    }
    if !workspaces.is_empty() {
        let _ = menu.append(&PredefinedMenuItem::separator());
    }
    let _ = menu.append(&MenuItem::with_id(
        MenuId::new("open-settings"),
        "Open Settings",
        true,
        None,
    ));
    let _ = menu.append(&MenuItem::with_id(
        MenuId::new("quit"),
        "Quit tili",
        true,
        None,
    ));
    menu
}

/// Renders `text` as a rounded-pill badge image (see `badge::image_for`)
/// and sets it as the status item button's image — `tray_icon`'s own
/// `set_title` only renders on macOS when an icon is also set, and a
/// plain title string couldn't do the knockout-pill look anyway, so this
/// goes through the raw `NSStatusItem` directly.
fn set_badge(tray: &TrayIcon, mtm: MainThreadMarker, text: &str) {
    let Some(status_item) = tray.ns_status_item() else {
        return;
    };
    if let Some(button) = status_item.button(mtm) {
        button.setImage(Some(&badge::image_for(text)));
    }
}

/// Shows/hides the whole status item — used so the badge disappears
/// entirely (not just shows stale/placeholder text) whenever the daemon
/// isn't reachable, including when it's stopped from outside
/// tili-menubar entirely (e.g. `tili stop` from the CLI).
fn set_visible(tray: &TrayIcon, _mtm: MainThreadMarker, visible: bool) {
    if let Some(status_item) = tray.ns_status_item() {
        status_item.setVisible(visible);
    }
}
