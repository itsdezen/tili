use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::{Parser, Subcommand, ValueEnum};
use tili_ipc::{
    Command, Direction, LayoutKind, MonitorInfo, PlacementInfo, Response, WindowInfo, WorkspaceInfo,
};

#[derive(Parser)]
#[command(
    name = "tili",
    about = "CLI for the tili tiling window manager daemon",
    version,
    disable_version_flag = true
)]
struct Cli {
    /// Print version
    #[arg(short = 'v', long = "version", action = clap::ArgAction::Version)]
    version: (),

    /// Defaults to `start` when no subcommand is given, so plain `tili`
    /// does the same thing as `tili start`.
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Clone, Copy, ValueEnum)]
enum DirArg {
    Left,
    Right,
    Up,
    Down,
}

impl From<DirArg> for Direction {
    fn from(dir: DirArg) -> Self {
        match dir {
            DirArg::Left => Direction::Left,
            DirArg::Right => Direction::Right,
            DirArg::Up => Direction::Up,
            DirArg::Down => Direction::Down,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum LayoutArg {
    Toggle,
    Tiles,
    Accordion,
    Horizontal,
    Vertical,
    ToggleOrientation,
}

#[derive(Clone, Copy, ValueEnum)]
enum OnOffArg {
    On,
    Off,
}

impl From<OnOffArg> for bool {
    fn from(state: OnOffArg) -> Self {
        matches!(state, OnOffArg::On)
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Install and start tili-daemon as a LaunchAgent — it keeps running in
    /// the background, restarts if it crashes, and starts automatically at
    /// login.
    Start,
    /// Stop tili-daemon and remove its LaunchAgent, so it doesn't come back
    /// until `tili start` is run again.
    Stop,
    /// Report whether the daemon is running and reachable.
    Status,
    /// Check whether the daemon is reachable.
    Ping,
    /// List currently known windows.
    ListWindows,
    /// Move focus to the window in the given direction.
    Focus { direction: DirArg },
    /// Move the focused window one step in the given direction, re-parenting
    /// it through the tree rather than just swapping places.
    Move { direction: DirArg },
    /// Wrap the focused window and its neighbor in the given direction into
    /// a new, perpendicular container.
    Join { direction: DirArg },
    /// Grow (positive) or shrink (negative) the focused window's share of
    /// its nearest tiled container.
    Resize { amount: f32 },
    /// List workspaces, marking which one is active.
    ListWorkspaces,
    /// Switch the active workspace. Errors if `name` isn't declared in
    /// config — workspaces are never created on the fly.
    Workspace { name: String },
    /// Move the focused window to another workspace without following it.
    MoveToWorkspace { name: String },
    /// Toggle, or explicitly set, the focused window's container layout.
    Layout {
        mode: LayoutArg,
        /// Target the workspace's root container instead of the focused
        /// window's immediate parent — still one container, not applied to
        /// every container at once.
        #[arg(long)]
        root: bool,
    },
    /// Cycle which connected monitor commands act on.
    FocusMonitor,
    /// List connected monitors, marking which one is focused.
    ListMonitors,
    /// Reset every child weight of the focused window's parent container
    /// (or the workspace root, if --root) evenly, undoing any manual
    /// resizes.
    Balance {
        #[arg(long)]
        root: bool,
    },
    /// Re-normalize the tree. A no-op today: Tree::normalize already runs
    /// after every mutation and already collapses stray one-child
    /// containers.
    Flatten,
    /// Toggle the focused window fullscreen.
    Fullscreen {
        /// Use macOS's own native fullscreen (a separate Space) instead of
        /// tili's own tiled fullscreen.
        #[arg(long)]
        native: bool,
    },
    /// Close the focused window (best-effort AXCloseButton press).
    Close,
    /// Focus (and raise) the first known window whose title or bundle id
    /// contains the given text.
    Summon { query: String },
    /// Move a workspace to a different monitor without switching focus to
    /// it. Target is a monitor id, "next", or "main". Defaults to whatever
    /// workspace is currently active on the focused monitor; pass
    /// `--workspace` to name a different (possibly parked) one.
    MoveWorkspaceToMonitor {
        target: String,
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Switch back to whichever workspace was active before this one.
    WorkspaceBack,
    /// Toggle the focused window between tiled and floating at runtime.
    SetFloating { state: OnOffArg },
    /// Fully tear down tili's local state: stops both LaunchAgents, then
    /// removes the config file, logs, IPC socket, and the Accessibility
    /// permission grant. Doesn't touch a Homebrew install itself — run
    /// `brew uninstall tili` separately if that's how tili was installed.
    Uninstall,
}

/// What shape of payload to expect back, so the CLI doesn't have to guess
/// from JSON structure alone.
enum ExpectedPayload {
    None,
    Windows,
    Workspaces,
    Monitors,
}

fn main() {
    let cli = Cli::parse();
    let defaulted_to_start = cli.command.is_none();
    let command = cli.command.unwrap_or(Commands::Start);

    // None of these three go through the generic send()/print_response
    // path below: Start/Stop never talk to the socket at all (they manage
    // the LaunchAgent directly), and Status wants its own wording instead
    // of the generic "couldn't reach daemon" error (which is a hard
    // failure for every other command, but an expected, calmly-reported
    // outcome here).
    match &command {
        Commands::Start => {
            // Only bare `tili` (no subcommand) needs confirming — an
            // explicit `tili start` is already an unambiguous request.
            if defaulted_to_start && !confirm_default_start() {
                println!("tili: cancelled");
                return;
            }
            start_daemon();
            return;
        }
        Commands::Stop => {
            stop_daemon();
            return;
        }
        Commands::Status => {
            print_status();
            return;
        }
        Commands::Uninstall => {
            uninstall();
            return;
        }
        _ => {}
    }

    let (command, expected) = match command {
        Commands::Ping => (Command::Ping, ExpectedPayload::None),
        Commands::ListWindows => (Command::ListWindows, ExpectedPayload::Windows),
        Commands::Focus { direction } => (Command::Focus(direction.into()), ExpectedPayload::None),
        Commands::Move { direction } => (Command::Move(direction.into()), ExpectedPayload::None),
        Commands::Join { direction } => (Command::Join(direction.into()), ExpectedPayload::None),
        Commands::Resize { amount } => (Command::ResizeRatio { amount }, ExpectedPayload::None),
        Commands::ListWorkspaces => (Command::ListWorkspaces, ExpectedPayload::Workspaces),
        Commands::Workspace { name } => (Command::WorkspaceSwitch(name), ExpectedPayload::None),
        Commands::MoveToWorkspace { name } => {
            (Command::MoveNodeToWorkspace(name), ExpectedPayload::None)
        }
        Commands::Layout { mode, root } => {
            let command = match mode {
                LayoutArg::Toggle => Command::LayoutToggle(root),
                LayoutArg::Tiles => Command::LayoutSet(LayoutKind::Tiles, root),
                LayoutArg::Accordion => Command::LayoutSet(LayoutKind::Accordion, root),
                LayoutArg::Horizontal => {
                    Command::OrientationSet(tili_ipc::OrientationKind::Horizontal, root)
                }
                LayoutArg::Vertical => {
                    Command::OrientationSet(tili_ipc::OrientationKind::Vertical, root)
                }
                LayoutArg::ToggleOrientation => Command::OrientationToggle(root),
            };
            (command, ExpectedPayload::None)
        }
        Commands::FocusMonitor => (Command::FocusMonitor, ExpectedPayload::None),
        Commands::ListMonitors => (Command::ListMonitors, ExpectedPayload::Monitors),
        Commands::Balance { root } => (Command::BalanceSizes { root }, ExpectedPayload::None),
        Commands::Flatten => (Command::Flatten, ExpectedPayload::None),
        Commands::Fullscreen { native } => {
            (Command::FullscreenToggle { native }, ExpectedPayload::None)
        }
        Commands::Close => (Command::Close, ExpectedPayload::None),
        Commands::Summon { query } => (Command::Summon(query), ExpectedPayload::None),
        Commands::MoveWorkspaceToMonitor { workspace, target } => {
            let target = parse_monitor_target(&target).unwrap_or_else(|| {
                eprintln!(
                    "tili: invalid monitor target '{target}' (expected a monitor id, 'next', or 'main')"
                );
                std::process::exit(1);
            });
            (
                Command::MoveWorkspaceToMonitor { workspace, target },
                ExpectedPayload::None,
            )
        }
        Commands::WorkspaceBack => (Command::WorkspaceBack, ExpectedPayload::None),
        Commands::SetFloating { state } => {
            (Command::SetFloating(state.into()), ExpectedPayload::None)
        }
        Commands::Start => unreachable!("handled above before the socket connection"),
        Commands::Stop => unreachable!("handled above before the socket connection"),
        Commands::Status => unreachable!("handled above before the socket connection"),
        Commands::Uninstall => unreachable!("handled above before the socket connection"),
    };

    match send(command) {
        Ok(response) => print_response(response, expected),
        Err(e) => {
            eprintln!(
                "tili: couldn't reach tili-daemon at {}: {e}\n\
                 (is the daemon running? try `tili start`)",
                tili_ipc::default_socket_path().display()
            );
            std::process::exit(1);
        }
    }
}

/// How long `send()` waits for the daemon before giving up — generous for
/// a one-shot interactive command (a healthy daemon responds near-
/// instantly), but still bounded so a command never hangs forever.
const SOCKET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Sends one length-prefixed JSON `Command` and reads back one
/// length-prefixed JSON `Response`. Synchronous/blocking, matching the
/// wire format documented on `tili_ipc::default_socket_path` — this CLI is
/// a short-lived one-shot process, so plain `std::io` is simpler than
/// pulling in an async runtime just to talk to the daemon.
///
/// A read/write timeout is required, not just nice-to-have: tili-daemon
/// can legitimately have its socket bound but not yet accepting
/// connections for up to a minute (waiting on a first-time Accessibility
/// grant — see `tili-daemon/src/main.rs`), so a successful `connect()`
/// doesn't guarantee a prompt response. Without a timeout, `read_exact`
/// would block indefinitely in that window instead of failing fast.
fn send(command: Command) -> io::Result<Response> {
    let mut stream = UnixStream::connect(tili_ipc::default_socket_path())?;
    let timeout = Some(SOCKET_TIMEOUT);
    stream.set_read_timeout(timeout)?;
    stream.set_write_timeout(timeout)?;

    let payload = serde_json::to_vec(&command)?;
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "command too large"))?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(&payload)?;

    let mut len_buf = [0_u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut response_buf = vec![0_u8; len];
    stream.read_exact(&mut response_buf)?;
    serde_json::from_slice(&response_buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn print_response(response: Response, expected: ExpectedPayload) {
    match response {
        Response::Ok => println!("ok"),
        Response::Err { message } => {
            eprintln!("tili: {message}");
            std::process::exit(1);
        }
        Response::OkWithPayload(payload) => match expected {
            ExpectedPayload::Windows => print_windows(payload),
            ExpectedPayload::Workspaces => print_workspaces(payload),
            ExpectedPayload::Monitors => print_monitors(payload),
            ExpectedPayload::None => println!("{payload}"),
        },
    }
}

fn print_windows(payload: serde_json::Value) {
    match serde_json::from_value::<Vec<WindowInfo>>(payload) {
        Ok(windows) if windows.is_empty() => println!("no windows found"),
        Ok(windows) => {
            for w in windows {
                let placement = placement_label(w.placement);
                println!(
                    "{:>10}  pid={:<8} {placement} {:.0}x{:.0}+{:.0}+{:.0}  {}",
                    w.id, w.pid, w.frame.width, w.frame.height, w.frame.x, w.frame.y, w.title
                );
            }
        }
        Err(_) => println!("(response payload not recognized)"),
    }
}

fn placement_label(placement: PlacementInfo) -> &'static str {
    match placement {
        PlacementInfo::Tiled => "tile ",
        PlacementInfo::Floating => "float",
        PlacementInfo::NativeFullscreen => "fullscr",
        PlacementInfo::Minimized => "min  ",
        PlacementInfo::HiddenApplication => "hidden",
        PlacementInfo::Popup => "popup",
    }
}

fn print_workspaces(payload: serde_json::Value) {
    match serde_json::from_value::<Vec<WorkspaceInfo>>(payload) {
        Ok(workspaces) => {
            for w in workspaces {
                let marker = if w.active { "*" } else { " " };
                let monitor = w
                    .monitor
                    .map(|id| format!("monitor={id}"))
                    .unwrap_or_default();
                println!(
                    "{marker} {:<20} {} window(s)  {monitor}",
                    w.name, w.window_count
                );
            }
        }
        Err(_) => println!("(response payload not recognized)"),
    }
}

const LAUNCH_AGENT_LABEL: &str = "com.tili.daemon";
/// The menu bar badge's own LaunchAgent — installed/removed alongside the
/// daemon's own by `start_daemon`/`stop_daemon`, so `tili start`/`tili
/// stop` is the single lifecycle control for "the whole tili experience,"
/// not just the daemon. Best-effort throughout: a menu bar badge failing
/// to install/start should never block or fail `tili start` itself, since
/// the daemon (and hotkeys/CLI) are already fully usable without it.
const MENUBAR_LAUNCH_AGENT_LABEL: &str = "com.tili.menubar";

fn launch_agent_path(label: &str) -> std::path::PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    std::path::PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist"))
}

/// A sibling binary ships alongside `tili` itself (both land in the same
/// bin directory, whether that's a Homebrew prefix or `cargo build`'s
/// `target/debug`) — resolved relative to this running binary rather than
/// relying on it being on `PATH`, which a LaunchAgent's minimal
/// environment doesn't guarantee.
fn sibling_binary_path(name: &str) -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|dir| dir.join(name)))
        .filter(|p| p.exists())
        .unwrap_or_else(|| std::path::PathBuf::from(name))
}

/// Parses `tili move-workspace-to-monitor <workspace> <target>`'s target
/// argument — a monitor id, or the literal `next`/`main`.
fn parse_monitor_target(s: &str) -> Option<tili_ipc::MonitorTarget> {
    match s {
        "next" => Some(tili_ipc::MonitorTarget::Next),
        "main" => Some(tili_ipc::MonitorTarget::Main),
        _ => s.parse::<u32>().ok().map(tili_ipc::MonitorTarget::Id),
    }
}

/// `tili status` — same underlying check as `tili ping`, worded for a
/// human glancing at it rather than for scripting. Distinguishes "not
/// running at all" (connection refused/socket missing) from "running but
/// hasn't finished starting yet" (connected, but timed out waiting for a
/// response — e.g. still in the bounded Accessibility-grant wait) rather
/// than reporting both the same way.
fn print_status() {
    match send(Command::Ping) {
        Ok(_) => println!(
            "tili-daemon is running (socket: {})",
            tili_ipc::default_socket_path().display()
        ),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            println!(
                "tili-daemon is running but hasn't finished starting yet (likely still \
                 waiting on a permission grant) — try again in a moment, or check \
                 ~/Library/Logs/tili/daemon.err.log"
            );
        }
        Err(_) => println!("tili-daemon is not running"),
    }
}

/// Guards against accidentally starting the daemon via a bare `tili` typo
/// (e.g. meaning to type `tili status` or `tili stop`) — since bare `tili`
/// now defaults to `start`, this requires an explicit Enter press first.
/// Ctrl-C (SIGINT) or Ctrl-D (EOF, `read_line` returning `Ok(0)`) both
/// cancel rather than proceed.
fn confirm_default_start() -> bool {
    print!("No subcommand given — press Enter to run `tili start` (Ctrl-C to cancel): ");
    let _ = io::stdout().flush();
    let mut input = String::new();
    matches!(io::stdin().read_line(&mut input), Ok(n) if n > 0)
}

/// `launchctl load` only registers the job and returns — it says nothing
/// about whether tili-daemon has actually finished its own startup
/// sequence (Input Monitoring/Accessibility checks) and is reachable yet.
/// Polls until the socket responds, or the LaunchAgent disappears (the
/// daemon found Accessibility not granted and stopped itself — see
/// `tili-daemon`'s `stop_self`), printing progress so the user isn't
/// staring at a silent terminal.
///
/// Ctrl-C here deliberately stops the daemon too (via `stop_daemon`),
/// rather than just abandoning this CLI-side wait — the daemon isn't
/// meant to keep running unattended in the background just because this
/// command was interrupted.
fn wait_for_daemon_ready() {
    let interrupted = Arc::new(AtomicBool::new(false));
    {
        let interrupted = interrupted.clone();
        let _ = ctrlc::set_handler(move || interrupted.store(true, Ordering::SeqCst));
    }

    print!("tili: waiting for tili-daemon to finish starting");
    let _ = io::stdout().flush();
    loop {
        if interrupted.load(Ordering::SeqCst) {
            println!();
            eprintln!("tili: interrupted — stopping tili-daemon.");
            stop_daemon();
            std::process::exit(130);
        }
        if daemon_is_reachable() {
            println!(" running.");
            return;
        }
        if !launch_agent_is_loaded(LAUNCH_AGENT_LABEL) {
            println!();
            eprintln!(
                "tili: tili-daemon stopped during startup (Accessibility permission is likely \
                 not granted yet) — check ~/Library/Logs/tili/daemon.err.log, grant the \
                 permission in System Settings > Privacy & Security > Accessibility, then run \
                 `tili start` again."
            );
            return;
        }
        print!(".");
        let _ = io::stdout().flush();
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

fn launch_agent_is_loaded(label: &str) -> bool {
    std::process::Command::new("launchctl")
        .args(["list", label])
        .output()
        .is_ok_and(|out| out.status.success())
}

/// A `Command::Ping` with short read/write timeouts, deliberately not
/// reusing `send()` — that function has no timeout, which is correct for
/// every other command (a real command should wait for its response), but
/// wrong here: this is polled in a loop precisely because the daemon might
/// not be accepting connections on the other end yet.
fn daemon_is_reachable() -> bool {
    let Ok(mut stream) = UnixStream::connect(tili_ipc::default_socket_path()) else {
        return false;
    };
    let timeout = Some(std::time::Duration::from_millis(300));
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);

    let Ok(payload) = serde_json::to_vec(&Command::Ping) else {
        return false;
    };
    let Ok(len) = u32::try_from(payload.len()) else {
        return false;
    };
    if stream.write_all(&len.to_be_bytes()).is_err() || stream.write_all(&payload).is_err() {
        return false;
    }

    let mut len_buf = [0_u8; 4];
    if stream.read_exact(&mut len_buf).is_err() {
        return false;
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut response_buf = vec![0_u8; len];
    stream.read_exact(&mut response_buf).is_ok()
}

/// Writes `label`'s LaunchAgent plist (`RunAtLoad`+`KeepAlive`, logging to
/// `~/Library/Logs/tili/{log_name}.{log,err.log}`) and `launchctl load`s
/// it. Shared by `start_daemon` (the daemon itself) and the menu bar
/// badge — same mechanism, different binary/label/log prefix.
fn install_launch_agent(label: &str, binary: &std::path::Path, log_name: &str) -> Result<(), ()> {
    let plist_path = launch_agent_path(label);
    let Some(parent) = plist_path.parent() else {
        return Err(());
    };
    if let Err(e) = std::fs::create_dir_all(parent) {
        eprintln!("tili: couldn't create {}: {e}", parent.display());
        return Err(());
    }

    let home = std::env::var("HOME").unwrap_or_default();
    let log_dir = format!("{home}/Library/Logs/tili");
    let _ = std::fs::create_dir_all(&log_dir);

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log_dir}/{log_name}.log</string>
    <key>StandardErrorPath</key>
    <string>{log_dir}/{log_name}.err.log</string>
</dict>
</plist>
"#,
        binary.display()
    );

    if let Err(e) = std::fs::write(&plist_path, plist) {
        eprintln!("tili: couldn't write {}: {e}", plist_path.display());
        return Err(());
    }

    match std::process::Command::new("launchctl")
        .args(["load", "-w"])
        .arg(&plist_path)
        .status()
    {
        Ok(status) if status.success() => {
            println!("tili: LaunchAgent installed at {}", plist_path.display());
            Ok(())
        }
        Ok(status) => {
            eprintln!("tili: `launchctl load` exited with {status}");
            Err(())
        }
        Err(e) => {
            eprintln!("tili: couldn't run launchctl: {e}");
            Err(())
        }
    }
}

/// `tili start` — installs tili-daemon's LaunchAgent (see
/// `install_launch_agent`) so `launchd`, not this short-lived CLI
/// process, owns it from here on, then does the same for the menu bar
/// badge. The badge is best-effort: if `tili-menubar` isn't present in
/// this build (or its LaunchAgent fails to install) the daemon is
/// already up and fully usable without it, so this doesn't fail `tili
/// start` overall.
fn start_daemon() {
    if install_launch_agent(
        LAUNCH_AGENT_LABEL,
        &sibling_binary_path("tili-daemon"),
        "daemon",
    )
    .is_err()
    {
        std::process::exit(1);
    }
    wait_for_daemon_ready();
    let _ = install_launch_agent(
        MENUBAR_LAUNCH_AGENT_LABEL,
        &sibling_binary_path("tili-menubar"),
        "menubar",
    );
}

/// `tili stop` — `launchctl unload`s and removes both the daemon's and
/// the menu bar badge's LaunchAgent plists, so neither comes back until
/// `tili start` runs again. Either being already absent is reported
/// calmly, not as an error — matches tili-menubar's own "Quit" action,
/// which shells out to this same command.
fn stop_daemon() {
    let daemon_plist = launch_agent_path(LAUNCH_AGENT_LABEL);
    let menubar_plist = launch_agent_path(MENUBAR_LAUNCH_AGENT_LABEL);
    if !daemon_plist.exists() && !menubar_plist.exists() {
        println!("tili: daemon is not running");
        return;
    }
    for plist_path in [&daemon_plist, &menubar_plist] {
        if !plist_path.exists() {
            continue;
        }
        let _ = std::process::Command::new("launchctl")
            .args(["unload", "-w"])
            .arg(plist_path)
            .status();
        if let Err(e) = std::fs::remove_file(plist_path) {
            eprintln!("tili: couldn't remove {}: {e}", plist_path.display());
        }
    }
    println!("tili: daemon stopped");
}

/// `~/.config/tili/tili.kdl` — duplicated from `tili_config::default_config_path`
/// rather than depending on `tili-config` for one 3-line function; same
/// tradeoff `tili-menubar` already made for the same reason (that crate
/// pulls in a KDL parser and an FSEvents-based file watcher, both
/// irrelevant here).
fn default_config_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    std::path::PathBuf::from(home).join(".config/tili/tili.kdl")
}

/// `tili uninstall` — a full teardown, unlike `tili stop` (which only
/// removes the LaunchAgents so a later `tili start` can bring things back
/// unchanged). Removes every file tili writes outside of a Homebrew
/// Cellar/prefix, plus the Accessibility grant, so a fresh `tili start`
/// afterward behaves exactly like a first-ever install — including
/// re-prompting for permission, which is how this actually gets verified.
fn uninstall() {
    stop_daemon();

    let config_path = default_config_path();
    if config_path.exists()
        && let Err(e) = std::fs::remove_file(&config_path)
    {
        eprintln!("tili: couldn't remove {}: {e}", config_path.display());
    }

    let home = std::env::var("HOME").unwrap_or_default();
    let logs_dir = std::path::PathBuf::from(&home).join("Library/Logs/tili");
    if logs_dir.exists()
        && let Err(e) = std::fs::remove_dir_all(&logs_dir)
    {
        eprintln!("tili: couldn't remove {}: {e}", logs_dir.display());
    }

    let socket_path = tili_ipc::default_socket_path();
    if socket_path.exists()
        && let Err(e) = std::fs::remove_file(&socket_path)
    {
        eprintln!("tili: couldn't remove {}: {e}", socket_path.display());
    }

    // Best-effort, same as tili-daemon's own reset_accessibility_tcc — a
    // no-op on an unsigned dev build, since tccutil keys off the bundle id
    // an ad-hoc-signed binary doesn't stably have.
    let _ = std::process::Command::new("tccutil")
        .args(["reset", "Accessibility", LAUNCH_AGENT_LABEL])
        .status();

    println!("tili: uninstalled — config, logs, socket, and Accessibility grant removed.");
    println!(
        "tili: if you installed via Homebrew, run `brew uninstall tili` to remove the binaries."
    );
}

fn print_monitors(payload: serde_json::Value) {
    match serde_json::from_value::<Vec<MonitorInfo>>(payload) {
        Ok(monitors) if monitors.is_empty() => println!("no monitors found"),
        Ok(monitors) => {
            for m in monitors {
                let marker = if m.focused { "*" } else { " " };
                let main = if m.is_main { "main" } else { "    " };
                let workspace = m.active_workspace.unwrap_or_else(|| "-".to_string());
                println!(
                    "{marker} {:<10} {main}  {:.0}x{:.0}+{:.0}+{:.0}  {workspace}",
                    m.id, m.frame.width, m.frame.height, m.frame.x, m.frame.y
                );
            }
        }
        Err(_) => println!("(response payload not recognized)"),
    }
}
