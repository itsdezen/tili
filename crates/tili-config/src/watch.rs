use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};

use notify::{EventKind, RecursiveMode, Watcher};

use crate::schema::{Config, parse};

/// Watches the config file for changes and sends a freshly-parsed `Config`
/// each time it's saved *and* parses successfully. A parse error is logged
/// to stderr and otherwise ignored — the caller keeps using whatever config
/// it already has, so a typo in `tili.kdl` can't take down (or silently
/// misconfigure) a running daemon.
///
/// Plain `std::sync::mpsc`, not async: this crate stays decoupled from any
/// particular runtime (see the "independent leaf" note in the architecture
/// docs) — `tili-daemon` bridges this into its `tokio::select!` loop itself,
/// the same way it bridges `tili-ax`'s NSWorkspace/AX event sources.
pub fn spawn_config_watcher(path: PathBuf) -> Receiver<Config> {
    let (config_tx, config_rx) = channel();

    std::thread::spawn(move || {
        let Some(parent) = path.parent().map(Path::to_path_buf) else {
            eprintln!(
                "tili-config: config path {} has no parent directory, not watching",
                path.display()
            );
            return;
        };
        // Best-effort: if the config directory doesn't exist yet, there's
        // nothing to watch and nothing to hot-reload — not a startup error.
        if !parent.exists() {
            return;
        }

        let (raw_tx, raw_rx) = channel();
        let watcher_result =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let _ = raw_tx.send(res);
            });
        let Ok(mut watcher) = watcher_result else {
            eprintln!("tili-config: failed to create file watcher");
            return;
        };

        // Watch the containing directory rather than the file itself: many
        // editors save by writing a temp file and renaming it over the
        // original, which some watch backends only surface as an event on
        // the directory, not a still-watched handle to the old file.
        if watcher.watch(&parent, RecursiveMode::NonRecursive).is_err() {
            eprintln!("tili-config: failed to watch {}", parent.display());
            return;
        }

        for res in raw_rx {
            let Ok(event) = res else { continue };
            if !matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                continue;
            }
            if !event.paths.iter().any(|p| p == &path) {
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(source) => match parse(&source) {
                    Ok(config) => {
                        if config_tx.send(config).is_err() {
                            break;
                        }
                    }
                    Err(e) => eprintln!("tili-config: failed to reload {}: {e}", path.display()),
                },
                Err(e) => eprintln!("tili-config: failed to read {}: {e}", path.display()),
            }
        }
    });

    config_rx
}
