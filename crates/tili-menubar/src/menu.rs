use std::collections::HashMap;

use objc2::MainThreadMarker;
use objc2_foundation::NSString;
use tray_icon::menu::{Menu, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

use crate::badge;

/// How many `main.rs` `DRAIN_INTERVAL` (0.05s) ticks between spinner
/// frames while disconnected — 4 ticks = 0.2s/frame, a typical CLI spinner
/// speed. Advancing off that existing timer (see `tick_spinner`) rather
/// than a dedicated one keeps this within the "no polling" invariant: the
/// timer already runs unconditionally today as a channel-drain no-op.
const SPINNER_TICKS_PER_FRAME: u32 = 4;

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
    /// The daemon's active keybindings mode (`"main"`/`"resize"`/
    /// `"manage"`/a custom name) — drives which glyph `badge::image_for`
    /// draws.
    pub mode: String,
    /// `None` specifically when this poll's `Command::MenubarStyle`
    /// request failed (distinct from the whole `Snapshot` being missing) —
    /// `apply_snapshot` then keeps whatever style was last applied instead
    /// of resetting to default, same "don't flash a broken state on a
    /// transient hiccup" reasoning as this struct itself.
    pub style: Option<tili_ipc::MenubarStyle>,
}

/// Builds the status item with a menu containing only the static "Open
/// Settings"/"Quit" items (the workspace section is added by the first
/// `apply_snapshot` call once real data arrives), hidden until the first
/// successful poll confirms the daemon is actually reachable — never
/// shows a placeholder badge that might be lying about the daemon being
/// up.
pub fn build_initial(mtm: MainThreadMarker) -> TrayIcon {
    let (menu, _items) = build_menu(None, &[], &tili_ipc::MenubarStyle::default());
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .build()
        .expect("build NSStatusItem");
    set_badge(
        &tray,
        mtm,
        "tili",
        "main",
        &tili_ipc::MenubarStyle::default(),
    );
    set_visible(&tray, mtm, false);
    tray
}

/// Polls `Command::ListMonitors` (for the focused monitor's current
/// workspace), `Command::ListWorkspaces` (the full switchable list),
/// `Command::CurrentMode` (for the badge glyph), and `Command::MenubarStyle`
/// (for badge/menu styling) — the first three are required for a `Some`
/// result (`None` on any of their transport/decode failures); the style
/// fetch is best-effort on its own (see `Snapshot::style`'s doc comment).
pub fn poll_daemon() -> Option<Snapshot> {
    let monitors: Vec<tili_ipc::MonitorInfo> = send_and_decode(tili_ipc::Command::ListMonitors)?;
    let workspaces: Vec<tili_ipc::WorkspaceInfo> =
        send_and_decode(tili_ipc::Command::ListWorkspaces)?;
    let mode: String = send_and_decode(tili_ipc::Command::CurrentMode)?;
    let style = send_and_decode(tili_ipc::Command::MenubarStyle);
    let current = monitors
        .into_iter()
        .find(|m| m.focused)
        .and_then(|m| m.active_workspace);
    Some(Snapshot {
        current,
        workspaces,
        mode,
        style,
    })
}

/// Sends `command` and decodes its `OkWithPayload` response as `T` — the
/// pattern all three `poll_daemon` requests share, `None` on any
/// transport/decode failure or non-payload response.
fn send_and_decode<T: serde::de::DeserializeOwned>(command: tili_ipc::Command) -> Option<T> {
    match crate::ipc::send(command).ok()? {
        tili_ipc::Response::OkWithPayload(v) => serde_json::from_value(v).ok(),
        _ => None,
    }
}

/// `(current, sorted workspace names, mode)` — enough to detect "does the
/// live `NSMenu` need a full rebuild" without needing `WorkspaceInfo` to
/// implement `PartialEq` itself. `window_count` is deliberately excluded
/// even though it's now shown in each item's label (see `apply_snapshot`):
/// it churns on every tick, and folding it into this key would rebuild the
/// whole `NSMenu` that often — this key exists specifically to avoid that
/// (see its own history: unconditional rebuild caused menu items to fire
/// clicks with zero user interaction). `window_count` instead updates via
/// `MenuItem::set_text` on the existing item, independent of this key.
/// `monitor` isn't shown in the menu at all, so it's excluded too.
pub type MenuKey = (Option<String>, Vec<String>, String);

pub fn menu_key(snapshot: &Snapshot) -> MenuKey {
    (
        snapshot.current.clone(),
        snapshot.workspaces.iter().map(|w| w.name.clone()).collect(),
        snapshot.mode.clone(),
    )
}

/// What was last actually applied to the status item — carried across
/// poll ticks (see `main.rs`) so `apply_snapshot` only touches AppKit
/// state that actually needs to change. `menu_key` avoids rebuilding a
/// live `NSMenu` for no reason (see its own history: doing that
/// unconditionally on every tick caused menu clicks to fire on their own
/// with zero user interaction); `visible` avoids redundant
/// `setVisible:` calls once the daemon becomes reachable for the first
/// time.
#[derive(Default)]
pub struct MenuState {
    key: Option<MenuKey>,
    visible: bool,
    /// `None` until the first poll result of any kind arrives. `Some(false)`
    /// while disconnected — drives the spinner (`tick_spinner`) and the
    /// "Connecting…" menu. `Some(true)` once real data has been shown at
    /// least once. Distinct from `visible`: both states are visible, this
    /// tracks *which* content is currently applied.
    connected: Option<bool>,
    spinner_frame: usize,
    spinner_tick: u32,
    /// Live `MenuItem` handles by workspace name, from the last rebuild —
    /// lets `apply_snapshot` update each label's window count in place
    /// every tick without going through `menu_key`'s rebuild gate. Cleared
    /// whenever the connecting menu replaces the workspace menu, since
    /// none of those handles are still in the live `NSMenu`.
    items: HashMap<String, MenuItem>,
    /// The last successfully fetched `MenubarStyle` — see
    /// `Snapshot::style`'s doc comment for why a single failed style fetch
    /// doesn't reset this to `MenubarStyle::default()`.
    style: tili_ipc::MenubarStyle,
}

/// Applies one poll result to the status item. `None` (daemon unreachable,
/// including "not running at all") switches to the connecting-spinner
/// badge/menu rather than hiding — a badge that's visibly retrying reads
/// better than one that vanishes with no explanation (see
/// `docs/architecture/tili-menubar.md`). `Some` shows real data, re-titling
/// the badge every tick (cheap), but only replacing the `NSMenu` itself
/// when `menu_key` actually differs from what's already applied.
pub fn apply_snapshot(
    tray: &TrayIcon,
    mtm: MainThreadMarker,
    snapshot: &Option<Snapshot>,
    state: &mut MenuState,
) {
    if !state.visible {
        set_visible(tray, mtm, true);
        state.visible = true;
    }

    let Some(snapshot) = snapshot else {
        if state.connected != Some(false) {
            state.connected = Some(false);
            state.spinner_frame = 0;
            state.spinner_tick = 0;
            tray.set_menu(Some(Box::new(build_menu_connecting())));
            state.items.clear();
            state.key = None;
        }
        set_badge_connecting(tray, mtm, state.spinner_frame);
        return;
    };
    state.connected = Some(true);
    if let Some(style) = &snapshot.style {
        state.style = style.clone();
    }

    let title = snapshot.current.as_deref().unwrap_or("tili");
    set_badge(tray, mtm, title, &snapshot.mode, &state.style);

    // Window counts churn every tick independent of `menu_key`, so update
    // each item's label in place — cheap, no `NSMenu` rebuild.
    for ws in &snapshot.workspaces {
        if let Some(item) = state.items.get(&ws.name) {
            let checked = snapshot.current.as_deref() == Some(ws.name.as_str());
            item.set_text(workspace_label(ws, checked, &state.style));
        }
    }

    let key = menu_key(snapshot);
    if state.key.as_ref() == Some(&key) {
        return;
    }
    let (menu, items) = build_menu(
        snapshot.current.as_deref(),
        &snapshot.workspaces,
        &state.style,
    );
    tray.set_menu(Some(Box::new(menu)));
    state.items = items;
    state.key = Some(key);
}

/// Advances the connecting-state spinner by one frame every
/// `SPINNER_TICKS_PER_FRAME` calls, a no-op while connected (or before the
/// first poll result ever arrives). Called from `main.rs`'s existing
/// `DRAIN_INTERVAL` timer on every tick, independent of whether that tick
/// had a channel message — the spinner needs to animate continuously
/// during `RECONNECT_BACKOFF`'s 1s gaps between poll attempts, not just
/// once per attempt.
pub fn tick_spinner(tray: &TrayIcon, mtm: MainThreadMarker, state: &mut MenuState) {
    if state.connected != Some(false) {
        return;
    }
    state.spinner_tick += 1;
    if state.spinner_tick.is_multiple_of(SPINNER_TICKS_PER_FRAME) {
        state.spinner_frame = state.spinner_frame.wrapping_add(1);
        set_badge_connecting(tray, mtm, state.spinner_frame);
    }
}

/// Shared by `build_menu` (initial label) and `apply_snapshot` (per-tick
/// update) so both agree on formatting. Omits the count suffix for an
/// empty workspace (to avoid a noisy "(0)" on every unused workspace) or
/// when `style.show_window_count` is off.
fn workspace_label(
    ws: &tili_ipc::WorkspaceInfo,
    checked: bool,
    style: &tili_ipc::MenubarStyle,
) -> String {
    let count_suffix = if style.show_window_count && ws.window_count > 0 {
        format!(" ({})", ws.window_count)
    } else {
        String::new()
    };
    if checked {
        format!("\u{2022} {}{}", ws.name, count_suffix)
    } else {
        format!("{}{}", ws.name, count_suffix)
    }
}

fn build_menu(
    current: Option<&str>,
    workspaces: &[tili_ipc::WorkspaceInfo],
    style: &tili_ipc::MenubarStyle,
) -> (Menu, HashMap<String, MenuItem>) {
    let menu = Menu::new();
    let mut items = HashMap::with_capacity(workspaces.len());
    for ws in workspaces {
        let checked = current == Some(ws.name.as_str());
        let label = workspace_label(ws, checked, style);
        let item = MenuItem::with_id(
            MenuId::new(format!("workspace:{}", ws.name)),
            &label,
            true,
            None,
        );
        let _ = menu.append(&item);
        items.insert(ws.name.clone(), item);
    }
    if !workspaces.is_empty() {
        let _ = menu.append(&PredefinedMenuItem::separator());
    }
    append_static_items(&menu);
    (menu, items)
}

/// The connecting-state menu — no workspace data exists yet, so a disabled
/// status line stands in for the workspace section; "Open Settings"/"Quit"
/// stay available since neither needs the daemon.
fn build_menu_connecting() -> Menu {
    let menu = Menu::new();
    let _ = menu.append(&MenuItem::with_id(
        MenuId::new("status"),
        "Connecting…",
        false,
        None,
    ));
    let _ = menu.append(&PredefinedMenuItem::separator());
    append_static_items(&menu);
    menu
}

fn append_static_items(menu: &Menu) {
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
}

/// Renders `text` as a rounded-pill badge image (see `badge::image_for`)
/// and sets it as the status item button's image — `tray_icon`'s own
/// `set_title` only renders on macOS when an icon is also set, and a
/// plain title string couldn't do the knockout-pill look anyway, so this
/// goes through the raw `NSStatusItem` directly.
fn set_badge(
    tray: &TrayIcon,
    mtm: MainThreadMarker,
    text: &str,
    mode: &str,
    style: &tili_ipc::MenubarStyle,
) {
    let Some(status_item) = tray.ns_status_item() else {
        return;
    };
    if let Some(button) = status_item.button(mtm) {
        button.setImage(Some(&badge::image_for(text, mode, style)));
        // Clears any leftover "connecting…" tooltip from a prior
        // disconnected state — a normal badge has nothing to explain.
        button.setToolTip(None);
    }
}

/// Renders the connecting-state spinner (see `badge::image_for_connecting`)
/// and sets a tooltip explaining the badge, since the spinner glyph alone
/// might not read as "reconnecting" rather than just an unusual mode icon.
fn set_badge_connecting(tray: &TrayIcon, mtm: MainThreadMarker, frame: usize) {
    let Some(status_item) = tray.ns_status_item() else {
        return;
    };
    if let Some(button) = status_item.button(mtm) {
        button.setImage(Some(&badge::image_for_connecting(frame)));
        button.setToolTip(Some(&NSString::from_str("tili: connecting…")));
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
