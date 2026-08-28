use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::{Parser, Subcommand, ValueEnum};
use tili_ipc::{
    Command, Direction, DoctorReport, LayoutKind, MonitorInfo, PlacementInfo, Response, WindowInfo,
    WorkspaceInfo,
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
    /// Checks daemon/menu-bar LaunchAgent state, the IPC socket, and the
    /// config file, then reports any problems found. A stale socket file,
    /// an installed-but-unloaded LaunchAgent, or one half of the
    /// daemon/menu-bar pair missing while the other is installed can all be
    /// auto-fixed (with confirmation, unless --fix is passed). A bad config
    /// file or a missing permission grant is reported only — never
    /// auto-fixed, since only the user can decide what they meant.
    Doctor {
        /// Apply fixes immediately instead of asking for confirmation
        /// first — still prints what changed, just skips the interactive
        /// gate (for scripting).
        #[arg(long)]
        fix: bool,
    },
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
    // Every subcommand eventually locates the config file, socket, or
    // LaunchAgent/log directories under `$HOME` (`default_config_path`,
    // `tili_ipc::default_socket_path`, `install_launch_agent`, ...) — each
    // of those resolves it with a bare `.expect`, so checking it once here
    // turns a raw panic (a stripped launchd environment, `env -i`, ...)
    // into a clear, actionable message before any of them run.
    if std::env::var("HOME").is_err() {
        eprintln!("tili: $HOME is not set — tili can't locate its config, socket, or log files.");
        std::process::exit(1);
    }

    let cli = Cli::parse();
    let defaulted_to_start = cli.command.is_none();
    let command = cli.command.unwrap_or(Commands::Start);

    // None of these go through the generic send()/print_response path
    // below: Start/Stop never talk to the socket at all (they manage the
    // LaunchAgent directly), Status wants its own wording instead of the
    // generic "couldn't reach daemon" error (which is a hard failure for
    // every other command, but an expected, calmly-reported outcome here),
    // and Doctor talks to the socket only optionally, as one of several
    // checks, rather than sending a single `Command` and printing its
    // response.
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
        Commands::Doctor { fix } => {
            doctor(*fix);
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
        Commands::Doctor { .. } => unreachable!("handled above before the socket connection"),
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

    write_framed(&mut stream, &command)?;
    let response_buf = read_framed(&mut stream)?;
    serde_json::from_slice(&response_buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Writes one length-prefixed JSON `Command` — the wire format documented
/// on `tili_ipc::default_socket_path`. Shared by `send()` and
/// `daemon_is_reachable()`, which frame identically and only differ in
/// timeout and how they surface an error.
fn write_framed(stream: &mut UnixStream, command: &Command) -> io::Result<()> {
    let payload = serde_json::to_vec(command)?;
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "command too large"))?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(&payload)
}

/// Reads one length-prefixed payload back — the other half of
/// `write_framed`, shared the same way.
fn read_framed(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut len_buf = [0_u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut response_buf = vec![0_u8; len];
    stream.read_exact(&mut response_buf)?;
    Ok(response_buf)
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
/// not just the daemon. Required, not best-effort: the daemon and the
/// badge are meant to run as a synchronized pair (never one without the
/// other), so a badge install failure rolls `tili start` back rather than
/// leaving the daemon running alone — see `start_daemon`. Runtime crashes
/// are handled separately: `tili-daemon`'s own shutdown paths tear this
/// LaunchAgent down too, and `tili-menubar` gives up and stops itself if
/// the daemon goes unreachable for a sustained period.
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
/// environment doesn't guarantee. Canonicalized first: Homebrew's `bin/`
/// prefix holds a symlink to `tili` *and* one to `tili-menubar` side by
/// side, so an unresolved `current_exe()` would find the sibling right
/// there and bake that symlink path into the LaunchAgent plist instead of
/// the real path inside `tili.app` — which is what actually carries the
/// bundle's name/icon in System Settings. Routed through
/// `homebrew_stable_equivalent` first so that, under Homebrew, the baked-in
/// path survives a future `brew upgrade` instead of pinning this exact
/// version forever (see that function's doc comment).
fn sibling_binary_path(name: &str) -> std::path::PathBuf {
    std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .ok()
        .map(|real| homebrew_stable_equivalent(&real).unwrap_or(real))
        .and_then(|p| p.parent().map(|dir| dir.join(name)))
        .filter(|p| p.exists())
        .unwrap_or_else(|| std::path::PathBuf::from(name))
}

/// Rewrites a canonicalized path that runs through Homebrew's Cellar
/// (`<prefix>/Cellar/tili/<version>/tili.app/Contents/MacOS/<bin>`) into the
/// equivalent path through `<prefix>/opt/tili` instead — the symlink
/// Homebrew relinks to whichever keg is current on every `brew upgrade`
/// (`bin/tili` gets the same treatment), unlike the version-pinned Cellar
/// path itself. Still lands inside `tili.app`'s bundle structure — the
/// property `sibling_binary_path`'s canonicalize actually needs for System
/// Settings/the menu bar to resolve tili's name/icon — just through one
/// stable symlink hop instead of zero, so a LaunchAgent plist built from it
/// keeps pointing at the right binary after a future upgrade instead of
/// caching this exact version forever (`post_install` can restart the
/// process via `KeepAlive`, but can't rewrite an already-loaded plist — see
/// `Formula/tili.rb`). Returns `None` for anything that doesn't match this
/// exact layout (a `cargo install`/dev build, or an `opt/tili` that doesn't
/// currently resolve back to this same keg), so the caller falls back to
/// the literal canonicalized path.
fn homebrew_stable_equivalent(real_exe: &std::path::Path) -> Option<std::path::PathBuf> {
    let comps: Vec<_> = real_exe.components().collect();
    let cellar_at = comps.iter().position(|c| c.as_os_str() == "Cellar")?;
    if comps.get(cellar_at + 1)?.as_os_str() != "tili" {
        return None;
    }
    let prefix: std::path::PathBuf = comps[..cellar_at].iter().collect();
    let keg: std::path::PathBuf = comps[..cellar_at + 3].iter().collect();
    let opt_tili = prefix.join("opt").join("tili");
    if std::fs::canonicalize(&opt_tili).ok()? != keg {
        return None;
    }
    let rest: std::path::PathBuf = comps[cellar_at + 3..].iter().collect();
    Some(opt_tili.join(rest))
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

    write_framed(&mut stream, &Command::Ping).is_ok() && read_framed(&mut stream).is_ok()
}

/// `daemon_is_reachable`, retried a few times with a short gap — a single
/// 300ms probe is too tight for a daemon that's alive but momentarily slow
/// to answer (every command, `Ping` included, runs a synchronous AX focus
/// resync first — see `dispatch()`'s doc comment — which can legitimately
/// take longer than that against a busy/unresponsive frontmost app). Used
/// only by `doctor()`'s "stale socket" check, whose `--fix` deletes the
/// socket file on a negative result — a false negative there doesn't just
/// misreport, it breaks IPC for a daemon that was never actually down.
fn daemon_is_reachable_retrying() -> bool {
    for attempt in 0..3 {
        if daemon_is_reachable() {
            return true;
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
    false
}

/// Writes `label`'s LaunchAgent plist (`RunAtLoad`+`KeepAlive`, logging to
/// `~/Library/Logs/tili/{log_name}.{log,err.log}`) and `launchctl load`s
/// it. Shared by `start_daemon` (the daemon itself) and the menu bar
/// badge — same mechanism, different binary/label/log prefix.
///
/// If `label` is already loaded (e.g. a previous `tili start` installed it
/// and its process is still running), unloads it first. `launchctl load`
/// on an already-loaded label doesn't apply a rewritten plist's contents —
/// launchd keeps using the definition it cached at the earlier `load` — it
/// just fails noisily (`Load failed: 5: Input/output error`) while still
/// exiting 0, which this function would otherwise report as success.
/// Unloading first avoids that noise and makes sure a changed
/// `ProgramArguments` path (e.g. after an upgrade) actually takes effect.
fn install_launch_agent(label: &str, binary: &std::path::Path, log_name: &str) -> Result<(), ()> {
    let plist_path = launch_agent_path(label);
    let Some(parent) = plist_path.parent() else {
        return Err(());
    };
    if let Err(e) = std::fs::create_dir_all(parent) {
        eprintln!("tili: couldn't create {}: {e}", parent.display());
        return Err(());
    }

    if launch_agent_is_loaded(label) {
        let _ = std::process::Command::new("launchctl")
            .args(["unload", "-w"])
            .arg(&plist_path)
            .status();
    }

    // `main()` already checked `$HOME` is set before any subcommand runs.
    let home = std::env::var("HOME").expect("HOME must be set");
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
/// badge. Both are required: the two are meant to run as a synchronized
/// pair, so a badge install failure stops the daemon back down (via
/// `stop_daemon`) instead of leaving it running alone.
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
    if install_launch_agent(
        MENUBAR_LAUNCH_AGENT_LABEL,
        &sibling_binary_path("tili-menubar"),
        "menubar",
    )
    .is_err()
    {
        eprintln!("tili: menu bar badge failed to start — stopping tili-daemon too.");
        stop_daemon();
        std::process::exit(1);
    }
}

/// `tili stop` — `launchctl unload`s and removes both the daemon's and
/// the menu bar badge's LaunchAgent plists, so neither comes back until
/// `tili start` runs again. Either being already absent is reported
/// calmly, not as an error — matches tili-menubar's own "Quit" action,
/// which shells out to this same command.
fn stop_daemon() {
    let daemon_plist = launch_agent_path(LAUNCH_AGENT_LABEL);
    let menubar_plist = launch_agent_path(MENUBAR_LAUNCH_AGENT_LABEL);
    // Checked via `launchctl`, not just whether the plist file exists on
    // disk: launchd caches a loaded job's definition independent of that
    // file (see `install_launch_agent`'s doc comment), so a plist deleted
    // out from under an already-loaded job would otherwise make this guard
    // report "not running" while the daemon (and/or badge) keeps running.
    let daemon_loaded = launch_agent_is_loaded(LAUNCH_AGENT_LABEL);
    let menubar_loaded = launch_agent_is_loaded(MENUBAR_LAUNCH_AGENT_LABEL);
    if !daemon_plist.exists() && !menubar_plist.exists() && !daemon_loaded && !menubar_loaded {
        println!("tili: daemon is not running");
        return;
    }
    for (label, plist_path) in [
        (LAUNCH_AGENT_LABEL, &daemon_plist),
        (MENUBAR_LAUNCH_AGENT_LABEL, &menubar_plist),
    ] {
        // Checked (and unloaded) before the plist-exists check below, not
        // gated on it: a job can be loaded with its plist file already gone
        // (see this function's opening comment) — skipping straight to
        // `continue` in that case would leave it running. Only unload if
        // launchd actually has the job loaded — e.g. a daemon that already
        // self-stopped (see tili-daemon's `stop_self`) leaves its plist
        // file behind but isn't loaded anymore, and `launchctl unload` on
        // that prints its own noisy "Unload failed: 5: Input/output error"
        // straight to stderr.
        if launch_agent_is_loaded(label) {
            let _ = std::process::Command::new("launchctl")
                .args(["unload", "-w"])
                .arg(plist_path)
                .status();
        }
        if !plist_path.exists() {
            continue;
        }
        remove_file_reporting(plist_path);
    }
    println!("tili: daemon stopped");
}

/// Removes `path`, printing `"tili: couldn't remove {path}: {e}"` on
/// failure — the pattern shared by `stop_daemon`'s plist removal and
/// `uninstall`'s config/socket removal. Returns whether an error was
/// printed, so `uninstall` can fold it into its own exit-status tracking;
/// `stop_daemon` ignores the return value, matching its original
/// best-effort handling.
fn remove_file_reporting(path: &std::path::Path) -> bool {
    if let Err(e) = std::fs::remove_file(path) {
        eprintln!("tili: couldn't remove {}: {e}", path.display());
        true
    } else {
        false
    }
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

/// Walks every ancestor directory of `path` (its parent, that parent's
/// parent, ...) up to the filesystem root, returning the first one that's
/// itself a symlink. Catches what `symlink_metadata` on `path` alone can't:
/// a dotfiles tool symlinking the whole `~/.config/tili` directory (or
/// anything above it) into a repo, rather than just `tili.kdl` inside it —
/// `symlink_metadata(path)` resolves transparently through a symlinked
/// parent, so the file itself still reports as an ordinary regular file.
fn symlinked_ancestor(path: &std::path::Path) -> Option<std::path::PathBuf> {
    path.parent()?
        .ancestors()
        .find(|dir| std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()))
        .map(std::path::Path::to_path_buf)
}

/// `tili uninstall` — a full teardown, unlike `tili stop` (which only
/// removes the LaunchAgents so a later `tili start` can bring things back
/// unchanged). Removes every file tili writes outside of a Homebrew
/// Cellar/prefix, plus the Accessibility grant, so a fresh `tili start`
/// afterward behaves exactly like a first-ever install — including
/// re-prompting for permission, which is how this actually gets verified.
fn uninstall() {
    stop_daemon();

    // Tracked so a script wrapping `tili uninstall` has a reliable way to
    // detect a leftover problem (a file that couldn't be removed) via exit
    // status, instead of every path here always exiting 0 regardless of
    // whether the `eprintln!`s below actually fired.
    let mut had_error = false;

    let config_path = default_config_path();
    // `symlink_metadata` doesn't follow the link, unlike `Path::exists()` —
    // needed to tell "a real file" from "a symlink a dotfiles manager
    // (stow, chezmoi, ...) points here." Removing the symlink itself would
    // still break that tool's arrangement even though it'd leave the real
    // target file untouched (`remove_file` on a symlink only unlinks the
    // link), so a symlinked config is left alone entirely.
    match std::fs::symlink_metadata(&config_path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            println!(
                "tili: {} is a symlink (likely managed by a dotfiles tool) — leaving it in place",
                config_path.display()
            );
        }
        Ok(_) => {
            if let Some(dir) = symlinked_ancestor(&config_path) {
                println!(
                    "tili: {} is inside {}, a symlink (likely managed by a dotfiles tool) — leaving it in place",
                    config_path.display(),
                    dir.display()
                );
            } else if remove_file_reporting(&config_path) {
                had_error = true;
            }
        }
        Err(_) => {}
    }

    // `main()` already checked `$HOME` is set before any subcommand runs.
    let home = std::env::var("HOME").expect("HOME must be set");
    let logs_dir = std::path::PathBuf::from(&home).join("Library/Logs/tili");
    if logs_dir.exists()
        && let Err(e) = std::fs::remove_dir_all(&logs_dir)
    {
        eprintln!("tili: couldn't remove {}: {e}", logs_dir.display());
        had_error = true;
    }

    let socket_path = tili_ipc::default_socket_path();
    if socket_path.exists() && remove_file_reporting(&socket_path) {
        had_error = true;
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

    if had_error {
        std::process::exit(1);
    }
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

/// One line of `tili doctor`'s report — `status` is `"ok"`, `"FAIL"`, or
/// `".."` (not applicable, e.g. "daemon not running yet"), kept to fixed
/// short strings so the columns line up without a table library.
fn doctor_line(label: &str, status: &str, detail: &str) {
    println!("{label:<20} {status:<5} {detail}");
}

/// `tili doctor` — checks daemon/menu-bar LaunchAgent state, the IPC
/// socket, and the config file; reports problems; and, for anything safely
/// automatable, offers to fix it. Deliberately does *not* try to fix a bad
/// config file or a missing permission grant — those need the user's own
/// judgment, so they're reported only, same call already made for
/// `tili-daemon`'s own config-reload/rule-skip warnings (see
/// `WmState::config_warnings`).
fn doctor(fix: bool) {
    println!("tili doctor: checking daemon, menu bar badge, and config...\n");

    let mut problems = 0_u32;
    let mut fixes: Vec<(String, Box<dyn FnOnce()>)> = Vec::new();

    let config_path = default_config_path();
    if !config_path.exists() {
        doctor_line(
            "config file",
            "..",
            &format!(
                "{} doesn't exist yet (tili start writes a starter)",
                config_path.display()
            ),
        );
    } else {
        match tili_config::load(&config_path) {
            Ok(_) => doctor_line(
                "config file",
                "ok",
                &format!("{} parses", config_path.display()),
            ),
            Err(e) => {
                problems += 1;
                doctor_line(
                    "config file",
                    "FAIL",
                    &format!("{} failed to parse: {e}", config_path.display()),
                );
            }
        }
    }

    let daemon_plist = launch_agent_path(LAUNCH_AGENT_LABEL);
    let menubar_plist = launch_agent_path(MENUBAR_LAUNCH_AGENT_LABEL);
    let daemon_installed = daemon_plist.exists();
    let menubar_installed = menubar_plist.exists();
    let daemon_loaded = daemon_installed && launch_agent_is_loaded(LAUNCH_AGENT_LABEL);
    let menubar_loaded = menubar_installed && launch_agent_is_loaded(MENUBAR_LAUNCH_AGENT_LABEL);

    doctor_report_launch_agent("daemon LaunchAgent", daemon_installed, daemon_loaded);
    doctor_report_launch_agent("menubar LaunchAgent", menubar_installed, menubar_loaded);

    // The daemon and the menu bar badge are meant to run as a synchronized
    // pair (see `MENUBAR_LAUNCH_AGENT_LABEL`'s doc comment) — one installed
    // without the other means a previous `tili start`/`stop` was
    // interrupted, or a LaunchAgent was removed by hand.
    if daemon_installed && !menubar_installed {
        problems += 1;
        doctor_line(
            "daemon/menubar pair",
            "FAIL",
            "daemon is installed but the menu bar badge isn't",
        );
        fixes.push((
            "install the missing menu bar badge LaunchAgent".to_string(),
            Box::new(|| {
                let _ = install_launch_agent(
                    MENUBAR_LAUNCH_AGENT_LABEL,
                    &sibling_binary_path("tili-menubar"),
                    "menubar",
                );
            }),
        ));
    } else if menubar_installed && !daemon_installed {
        problems += 1;
        doctor_line(
            "daemon/menubar pair",
            "FAIL",
            "the menu bar badge is installed but the daemon isn't",
        );
        fixes.push((
            "install the missing daemon LaunchAgent".to_string(),
            Box::new(|| {
                let _ = install_launch_agent(
                    LAUNCH_AGENT_LABEL,
                    &sibling_binary_path("tili-daemon"),
                    "daemon",
                );
            }),
        ));
    } else if daemon_installed {
        doctor_line("daemon/menubar pair", "ok", "both installed");
    }

    if daemon_installed && !daemon_loaded {
        problems += 1;
        fixes.push((
            "reload the daemon's LaunchAgent".to_string(),
            Box::new(move || {
                let _ = std::process::Command::new("launchctl")
                    .args(["load", "-w"])
                    .arg(&daemon_plist)
                    .status();
            }),
        ));
    }
    if menubar_installed && !menubar_loaded {
        problems += 1;
        fixes.push((
            "reload the menu bar badge's LaunchAgent".to_string(),
            Box::new(move || {
                let _ = std::process::Command::new("launchctl")
                    .args(["load", "-w"])
                    .arg(&menubar_plist)
                    .status();
            }),
        ));
    }

    let socket_path = tili_ipc::default_socket_path();
    let reachable = daemon_is_reachable_retrying();
    if socket_path.exists() && !reachable {
        problems += 1;
        doctor_line(
            "IPC socket",
            "FAIL",
            &format!(
                "{} exists but nothing is listening (stale, from an unclean shutdown)",
                socket_path.display()
            ),
        );
        fixes.push((
            "remove the stale IPC socket file".to_string(),
            Box::new(move || {
                let _ = std::fs::remove_file(&socket_path);
            }),
        ));
    } else if reachable {
        doctor_line(
            "IPC socket",
            "ok",
            &format!("daemon reachable at {}", socket_path.display()),
        );
    } else {
        doctor_line("IPC socket", "..", "not present (daemon not running)");
    }

    // Permission grants and the last config load's semantic warnings both
    // live only in the daemon's own process — see `Command::Doctor`'s doc
    // comment for why this doesn't duplicate that check here instead.
    if reachable {
        match send(Command::Doctor) {
            Ok(Response::OkWithPayload(payload)) => {
                match serde_json::from_value::<DoctorReport>(payload) {
                    Ok(report) => {
                        if report.accessibility_granted {
                            doctor_line("accessibility", "ok", "granted");
                        } else {
                            problems += 1;
                            doctor_line(
                                "accessibility",
                                "FAIL",
                                "not granted — grant it in System Settings > Privacy & Security > \
                             Accessibility",
                            );
                        }
                        if report.input_monitoring_granted {
                            doctor_line("input monitoring", "ok", "granted");
                        } else {
                            problems += 1;
                            doctor_line(
                                "input monitoring",
                                "FAIL",
                                "not granted — grant it in System Settings > Privacy & Security > \
                             Input Monitoring",
                            );
                        }
                        if report.config_warnings.is_empty() {
                            doctor_line("config warnings", "ok", "none from the last load");
                        } else {
                            problems += report.config_warnings.len() as u32;
                            doctor_line(
                                "config warnings",
                                "FAIL",
                                &format!("{} from the last load:", report.config_warnings.len()),
                            );
                            for warning in &report.config_warnings {
                                println!("                            - {warning}");
                            }
                        }
                    }
                    Err(_) => {
                        doctor_line("doctor report", "..", "couldn't parse the daemon's reply")
                    }
                }
            }
            _ => doctor_line(
                "doctor report",
                "..",
                "daemon didn't answer Command::Doctor",
            ),
        }
    } else {
        doctor_line(
            "permissions",
            "..",
            "can't check without a running daemon — run `tili start`",
        );
    }

    println!();
    // Captured before `fixes` is consumed below — how many of `problems`
    // this run can actually resolve; the rest (a bad config file, a missing
    // permission grant, ...) are report-only by design (see this function's
    // own doc comment) and always remain, so they factor into the exit code
    // even after every fixable problem gets applied.
    let auto_fixable = fixes.len() as u32;
    if fixes.is_empty() {
        if problems == 0 {
            println!("tili doctor: no problems found.");
        } else {
            println!("tili doctor: found {problems} problem(s), none auto-fixable — see above.");
            std::process::exit(1);
        }
        return;
    }

    println!("tili doctor: found {problems} problem(s), {auto_fixable} auto-fixable:");
    for (desc, _) in &fixes {
        println!("  - {desc}");
    }

    if !fix {
        print!("\nPress Enter to apply the fix(es) above (Ctrl-C to cancel): ");
        let _ = io::stdout().flush();
        let mut input = String::new();
        if !matches!(io::stdin().read_line(&mut input), Ok(n) if n > 0) {
            println!("tili: cancelled — no changes made.");
            std::process::exit(1);
        }
    }

    println!();
    for (desc, apply) in fixes {
        apply();
        println!("tili: fixed — {desc}");
    }

    // A problem left over after every auto-fixable one was just applied is
    // one of the report-only kinds (bad config, missing permission grant)
    // — still unresolved, so a script wrapping `tili doctor --fix` can tell
    // via exit status.
    if problems > auto_fixable {
        std::process::exit(1);
    }
}

fn doctor_report_launch_agent(label: &str, installed: bool, loaded: bool) {
    if !installed {
        doctor_line(label, "..", "not installed");
    } else if loaded {
        doctor_line(label, "ok", "installed and loaded");
    } else {
        doctor_line(label, "FAIL", "installed but not loaded");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tili-cli-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // `std::env::temp_dir()` runs through `/var` -> `/private/var` on
        // macOS, itself a symlink — canonicalizing here keeps that OS-level
        // detail from masquerading as the thing these tests are actually
        // checking for.
        std::fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn symlinked_ancestor_finds_a_symlinked_grandparent() {
        let root = scratch_dir("symlinked-ancestor-grandparent");
        let real_target = root.join("dotfiles/tili/.config/tili");
        std::fs::create_dir_all(&real_target).unwrap();
        let config_dir_link = root.join("config-link");
        std::os::unix::fs::symlink(&real_target, &config_dir_link).unwrap();
        let config_path = config_dir_link.join("tili.kdl");
        std::fs::write(&config_path, "").unwrap();

        assert_eq!(symlinked_ancestor(&config_path), Some(config_dir_link));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn symlinked_ancestor_is_none_for_an_ordinary_path() {
        let root = scratch_dir("symlinked-ancestor-ordinary");
        let config_path = root.join(".config/tili/tili.kdl");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "").unwrap();

        assert_eq!(symlinked_ancestor(&config_path), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn homebrew_stable_equivalent_rewrites_through_the_opt_symlink() {
        let root = scratch_dir("homebrew-stable-rewrite");
        let daemon = root.join("Cellar/tili/0.5.0/tili.app/Contents/MacOS/tili-daemon");
        std::fs::create_dir_all(daemon.parent().unwrap()).unwrap();
        std::fs::write(&daemon, "").unwrap();
        std::fs::create_dir_all(root.join("opt")).unwrap();
        std::os::unix::fs::symlink(root.join("Cellar/tili/0.5.0"), root.join("opt/tili")).unwrap();

        let resolved = homebrew_stable_equivalent(&daemon).unwrap();
        assert_eq!(
            resolved,
            root.join("opt/tili/tili.app/Contents/MacOS/tili-daemon")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn homebrew_stable_equivalent_survives_a_simulated_upgrade() {
        let root = scratch_dir("homebrew-stable-upgrade");
        let old_daemon = root.join("Cellar/tili/0.5.0/tili.app/Contents/MacOS/tili-daemon");
        std::fs::create_dir_all(old_daemon.parent().unwrap()).unwrap();
        std::fs::write(&old_daemon, "").unwrap();
        std::fs::create_dir_all(root.join("opt")).unwrap();
        std::os::unix::fs::symlink(root.join("Cellar/tili/0.5.0"), root.join("opt/tili")).unwrap();

        // Computed once, as `tili start` would when writing the LaunchAgent
        // plist while running as v0.5.0.
        let plist_path = homebrew_stable_equivalent(&old_daemon).unwrap();
        assert_eq!(
            plist_path,
            root.join("opt/tili/tili.app/Contents/MacOS/tili-daemon")
        );

        // Mirrors exactly what `brew upgrade`'s `finish` step does: install
        // the new keg, then relink `opt/tili` to it.
        let new_daemon = root.join("Cellar/tili/0.5.1/tili.app/Contents/MacOS/tili-daemon");
        std::fs::create_dir_all(new_daemon.parent().unwrap()).unwrap();
        std::fs::write(&new_daemon, "").unwrap();
        std::fs::remove_file(root.join("opt/tili")).unwrap();
        std::os::unix::fs::symlink(root.join("Cellar/tili/0.5.1"), root.join("opt/tili")).unwrap();

        // The exact path string already baked into a plist from before the
        // upgrade — unchanged, never rewritten — now resolves straight
        // through to the new binary, which is the whole point: no plist
        // rewrite needed for `post_install`'s restart to land on it.
        assert_eq!(
            std::fs::canonicalize(&plist_path).unwrap(),
            std::fs::canonicalize(&new_daemon).unwrap()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn homebrew_stable_equivalent_is_none_when_opt_does_not_match() {
        let root = scratch_dir("homebrew-stable-no-opt");
        let daemon = root.join("Cellar/tili/0.5.0/tili.app/Contents/MacOS/tili-daemon");
        std::fs::create_dir_all(daemon.parent().unwrap()).unwrap();
        std::fs::write(&daemon, "").unwrap();
        // No `opt/tili` symlink created at all.

        assert_eq!(homebrew_stable_equivalent(&daemon), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn homebrew_stable_equivalent_is_none_for_a_non_cellar_path() {
        let root = scratch_dir("homebrew-stable-dev-build");
        let daemon = root.join("target/debug/tili-daemon");
        std::fs::create_dir_all(daemon.parent().unwrap()).unwrap();
        std::fs::write(&daemon, "").unwrap();

        assert_eq!(homebrew_stable_equivalent(&daemon), None);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
