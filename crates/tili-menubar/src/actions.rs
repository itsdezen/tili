use std::path::PathBuf;

use tray_icon::menu::MenuId;

/// `~/.config/tili/tili.kdl` — duplicated from `tili_config::default_config_path`
/// rather than depending on `tili-config` for one 3-line function; that
/// crate pulls in a KDL parser and `notify`'s FSEvents stack, both
/// irrelevant to this binary. Same precedent as `ipc::send`'s duplication.
fn default_config_path() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    PathBuf::from(home).join(".config/tili/tili.kdl")
}

/// `tili` ships alongside `tili-menubar` (same `Contents/MacOS/` in the
/// packaged app, same `target/debug`/`target/release` dir in a local
/// build) — mirrors `tili-cli`'s own `daemon_binary_path()` exactly,
/// resolved relative to this running binary rather than relying on `tili`
/// being on `PATH`.
fn tili_binary_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|dir| dir.join("tili")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("tili"))
}

/// Opens the config file, trying each of these in order until one
/// actually succeeds (spawns *and* exits zero):
/// 1. `$EDITOR`, if set — a GUI editor like `code`/`subl` works fine
///    spawned directly; a terminal editor like `vim` won't have a TTY to
///    attach to from this GUI process and will typically exit non-zero
///    immediately, which is exactly the "didn't work, try the next
///    option" signal this cascade needs — no special-casing which editors
///    are GUI vs. terminal.
/// 2. `open <path>` — Launch Services' registered default for `.kdl`, if
///    any app claims it.
/// 3. `open -a TextEdit <path>` — ships on every Mac, so this step can't
///    itself fail to find an app; the last resort that guarantees the
///    file actually opens in *something*.
fn open_settings() {
    let path = default_config_path();

    if let Ok(editor) = std::env::var("EDITOR")
        && !editor.is_empty()
        && succeeded(std::process::Command::new(editor).arg(&path).status())
    {
        return;
    }
    if succeeded(std::process::Command::new("open").arg(&path).status()) {
        return;
    }
    let _ = std::process::Command::new("open")
        .args(["-a", "TextEdit"])
        .arg(&path)
        .status();
}

fn succeeded(result: std::io::Result<std::process::ExitStatus>) -> bool {
    result.is_ok_and(|status| status.success())
}

/// Dispatches a menu click by its id: `workspace:<name>` switches to that
/// workspace, `open-settings` opens the config file with `$EDITOR` (no
/// tili-side editor setting — the environment variable is already the
/// standard place for this), `quit` stops the whole daemon (via `tili
/// stop`, not just this process) and then exits this process too — a
/// menu bar badge with no daemon behind it has nothing useful left to
/// show.
pub fn handle(id: &MenuId) {
    let id: &str = id.as_ref();
    eprintln!("tili-menubar: menu event id={id:?}");
    if let Some(name) = id.strip_prefix("workspace:") {
        let _ = crate::ipc::send(tili_ipc::Command::WorkspaceSwitch(name.to_string()));
    } else if id == "open-settings" {
        open_settings();
    } else if id == "quit" {
        let _ = std::process::Command::new(tili_binary_path())
            .arg("stop")
            .status();
        std::process::exit(0);
    }
}
