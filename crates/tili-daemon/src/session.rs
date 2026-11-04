//! Persisted session state: a versioned, `WindowId`-based snapshot of every
//! managed workspace/placement, plus the pure matching planner used to
//! reconcile it against the windows that actually reopen.
//!
//! This module deliberately contains no `AxWindow`/`WmState` code — the
//! daemon owns extraction and application, while this layer stays
//! unit-testable without macOS. See `docs/architecture/tili-daemon.md`'s
//! session-state section.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tili_tree::{TreeAnchor, TreeSnapshot, WindowId};

pub const SESSION_VERSION: u32 = 1;
pub const DEFAULT_STATE_FILE_NAME: &str = "session.json";

/// A rectangle in the same coordinate space as `tili_tree::Rect`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl From<tili_tree::Rect> for SavedRect {
    fn from(rect: tili_tree::Rect) -> Self {
        Self {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        }
    }
}

impl SavedRect {
    pub fn to_rect(self) -> tili_tree::Rect {
        tili_tree::Rect {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
    }
}

/// A monitor as it existed when the session was saved. `id_hint` is only a
/// hint: `CGDirectDisplayID` can change across sleep/hot-plug, so restore
/// prefers a frame-origin match and falls back to the main display.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedMonitor {
    pub id_hint: u32,
    pub frame: SavedRect,
    pub is_main: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedMonitorRef {
    pub id_hint: u32,
    pub frame: SavedRect,
    pub is_main: bool,
}

impl From<&SavedMonitor> for SavedMonitorRef {
    fn from(monitor: &SavedMonitor) -> Self {
        Self {
            id_hint: monitor.id_hint,
            frame: monitor.frame,
            is_main: monitor.is_main,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedActiveWorkspace {
    pub monitor: SavedMonitorRef,
    pub workspace: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedWorkspace {
    pub name: String,
    pub tree: TreeSnapshot,
    pub focus: Option<WindowId>,
    pub fullscreen: Option<WindowId>,
    /// Last monitor this workspace was shown on, for an inactive workspace
    /// that has no entry in `active_workspaces`.
    #[serde(default)]
    pub monitor: Option<SavedMonitorRef>,
}

/// A window's placement state, serialized independently of the live
/// `PlacementKind` so the schema can evolve without exposing daemon-private
/// types.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SavedPlacement {
    Tiled,
    Floating { manual: Option<SavedFloatGeometry> },
    NativeFullscreen { restore: SavedRestore },
    Minimized { restore: SavedRestore },
    HiddenApplication { restore: SavedRestore },
    Popup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SavedRestore {
    Tiled,
    Floating,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedFloatGeometry {
    pub rel_x: f64,
    pub rel_y: f64,
    pub rel_w: f64,
    pub rel_h: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SavedWindowKind {
    Standard,
    Dialog,
    Popup,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedWindow {
    pub id: WindowId,
    pub pid: i32,
    pub bundle_id: String,
    pub title: String,
    pub kind: SavedWindowKind,
    pub workspace: String,
    pub placement: SavedPlacement,
    /// Only meaningful for a window currently in a special placement
    /// (`NativeFullscreen`/`Minimized`/`HiddenApplication`): where it
    /// should return to when it becomes normal again.
    pub anchor: Option<TreeAnchor>,
    /// Manual floating geometry preserved across a special-state round
    /// trip (a window minimized while floating should come back at the
    /// user's own size, not a freshly rule-derived one).
    pub special_manual: Option<SavedFloatGeometry>,
    /// Best-effort geometry hint, used to place a matched window
    /// immediately at startup before the final tree relayout.
    pub frame: Option<SavedRect>,
    pub monitor: Option<SavedMonitorRef>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedSession {
    pub version: u32,
    pub saved_at_unix_ms: u64,
    pub monitors: Vec<SavedMonitor>,
    pub focused_monitor: Option<SavedMonitorRef>,
    pub active_workspaces: Vec<SavedActiveWorkspace>,
    pub previous_workspace: Option<String>,
    pub workspaces: Vec<SavedWorkspace>,
    pub windows: Vec<SavedWindow>,
}

impl SavedSession {
    pub fn new(
        monitors: Vec<SavedMonitor>,
        focused_monitor: Option<SavedMonitorRef>,
        active_workspaces: Vec<SavedActiveWorkspace>,
        previous_workspace: Option<String>,
        workspaces: Vec<SavedWorkspace>,
        windows: Vec<SavedWindow>,
    ) -> Self {
        Self {
            version: SESSION_VERSION,
            saved_at_unix_ms: unix_millis(),
            monitors,
            focused_monitor,
            active_workspaces,
            previous_workspace,
            workspaces,
            windows,
        }
    }
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// The result of trying to load a saved session. Every failure is
/// non-fatal to the daemon — a missing/corrupt/newer state file simply
/// means "start without restoring."
#[derive(Clone, Debug, PartialEq)]
pub enum LoadOutcome {
    Loaded(SavedSession),
    Missing,
    Corrupt(String),
    UnsupportedVersion(u32),
}

/// Owns the session file path and the atomic write/backup policy.
#[derive(Clone, Debug)]
pub struct SessionStore {
    path: PathBuf,
    backup: PathBuf,
}

impl SessionStore {
    pub fn new(path: PathBuf) -> Self {
        let backup = path.with_extension("json.bak");
        Self { path, backup }
    }

    /// `~/Library/Application Support/tili/session.json`, the macOS-native
    /// state location. Config stays in `~/.config/tili`; state is not
    /// config and must be easy to remove on uninstall.
    pub fn default_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home)
            .join("Library/Application Support/tili")
            .join(DEFAULT_STATE_FILE_NAME)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    pub fn backup_path(&self) -> &Path {
        &self.backup
    }

    pub fn load(&self) -> LoadOutcome {
        match read_session(&self.path) {
            Ok(Some(session)) => return LoadOutcome::Loaded(session),
            Ok(None) => {}
            Err(ReadError::Version(version)) => {
                return LoadOutcome::UnsupportedVersion(version);
            }
            Err(ReadError::Io(message) | ReadError::Parse(message)) => {
                // Try the last good backup before declaring the file
                // corrupt; the primary may have been torn by a crash.
                return match read_session(&self.backup) {
                    Ok(Some(session)) => LoadOutcome::Loaded(session),
                    _ => LoadOutcome::Corrupt(message),
                };
            }
        }
        match read_session(&self.backup) {
            Ok(Some(session)) => LoadOutcome::Loaded(session),
            Ok(None) | Err(_) => LoadOutcome::Missing,
        }
    }

    /// Writes `session` atomically: temp file, fsync, rotate the old file
    /// to `.bak`, then rename the temp file into place. A crash can leave
    /// either the old file or `.bak` readable, never a partial primary.
    pub fn save(&self, session: &SavedSession) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(session)
            .map_err(|error| io::Error::other(error.to_string()))?;
        let tmp = self.path.with_extension("json.tmp");
        {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(&json)?;
            file.sync_all()?;
        }
        set_private_permissions(&tmp);

        if self.path.exists() {
            // Best-effort backup rotation; a failure to rotate must not
            // prevent the new state from being written.
            let _ = fs::rename(&self.path, &self.backup);
        }
        fs::rename(&tmp, &self.path)?;
        set_private_permissions(&self.path);
        Ok(())
    }
}

enum ReadError {
    Io(String),
    Parse(String),
    Version(u32),
}

fn read_session(path: &Path) -> Result<Option<SavedSession>, ReadError> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ReadError::Io(format!("{}: {error}", path.display()))),
    };
    let session: SavedSession = serde_json::from_slice(&data)
        .map_err(|error| ReadError::Parse(format!("{}: {error}", path.display())))?;
    if session.version != SESSION_VERSION {
        return Err(ReadError::Version(session.version));
    }
    Ok(Some(session))
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) {}

/// Plain identity of a currently-open window, used by the pure matching
/// planner. `kind` is tili's existing `WindowKind` classification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentWindowIdentity {
    pub id: WindowId,
    pub pid: i32,
    pub bundle_id: String,
    pub title: String,
    pub kind: SavedWindowKind,
}

impl CurrentWindowIdentity {
    pub fn new(
        id: WindowId,
        pid: i32,
        bundle_id: &str,
        title: &str,
        kind: SavedWindowKind,
    ) -> Self {
        Self {
            id,
            pid,
            bundle_id: bundle_id.to_string(),
            title: title.to_string(),
            kind,
        }
    }
}

/// Greedy, conservative matcher: hard identity first, then a fallback that
/// is only used when exactly one saved window can match. A wrong placement
/// is worse than no restore, so any ambiguity is skipped.
#[derive(Debug)]
pub struct RestoreMatcher {
    saved: Vec<SavedWindow>,
    claimed_saved: HashSet<usize>,
    claimed_current: HashSet<WindowId>,
    matched: Vec<(usize, WindowId)>,
}

impl RestoreMatcher {
    pub fn new(saved: &[SavedWindow]) -> Self {
        Self {
            saved: saved.to_vec(),
            claimed_saved: HashSet::new(),
            claimed_current: HashSet::new(),
            matched: Vec::new(),
        }
    }

    /// Attempts to match one current window. Returns the saved window's
    /// index and a clone of its placement data when unambiguous.
    pub fn try_match(&mut self, current: &CurrentWindowIdentity) -> Option<(usize, SavedWindow)> {
        if self.claimed_current.contains(&current.id) {
            return None;
        }

        let hard: Vec<usize> = self
            .saved
            .iter()
            .enumerate()
            .filter(|(idx, saved)| {
                !self.claimed_saved.contains(idx)
                    && saved.id == current.id
                    && saved.pid == current.pid
                    && saved.bundle_id == current.bundle_id
            })
            .map(|(idx, _)| idx)
            .collect();
        if hard.len() == 1 {
            return Some(self.claim(hard[0], current.id));
        }

        let fallback: Vec<usize> = self
            .saved
            .iter()
            .enumerate()
            .filter(|(idx, saved)| {
                !self.claimed_saved.contains(idx)
                    && saved.bundle_id == current.bundle_id
                    && !saved.title.is_empty()
                    && saved.title == current.title
                    && saved.kind == current.kind
            })
            .map(|(idx, _)| idx)
            .collect();
        if fallback.len() == 1 {
            return Some(self.claim(fallback[0], current.id));
        }

        None
    }

    fn claim(&mut self, saved_idx: usize, current_id: WindowId) -> (usize, SavedWindow) {
        self.claimed_saved.insert(saved_idx);
        self.claimed_current.insert(current_id);
        self.matched.push((saved_idx, current_id));
        (saved_idx, self.saved[saved_idx].clone())
    }

    pub fn remaining_normal(&self) -> usize {
        self.saved
            .iter()
            .enumerate()
            .filter(|(idx, saved)| {
                !self.claimed_saved.contains(idx)
                    && matches!(
                        saved.placement,
                        SavedPlacement::Tiled | SavedPlacement::Floating { .. }
                    )
            })
            .count()
    }

    pub fn unmatched_special(&self) -> Vec<SavedWindow> {
        self.saved
            .iter()
            .enumerate()
            .filter(|(idx, saved)| {
                !self.claimed_saved.contains(idx)
                    && matches!(
                        saved.placement,
                        SavedPlacement::NativeFullscreen { .. }
                            | SavedPlacement::Minimized { .. }
                            | SavedPlacement::HiddenApplication { .. }
                    )
            })
            .map(|(_, saved)| saved.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved_window(id: WindowId, placement: SavedPlacement) -> SavedWindow {
        SavedWindow {
            id,
            pid: id as i32,
            bundle_id: "com.example.app".to_string(),
            title: format!("window {id}"),
            kind: SavedWindowKind::Standard,
            workspace: "main".to_string(),
            placement,
            anchor: None,
            special_manual: None,
            frame: None,
            monitor: None,
        }
    }

    fn identity(id: WindowId, pid: i32) -> CurrentWindowIdentity {
        CurrentWindowIdentity::new(
            id,
            pid,
            "com.example.app",
            &format!("window {id}"),
            SavedWindowKind::Standard,
        )
    }

    #[test]
    fn matcher_prefers_hard_identity() {
        let saved = vec![
            saved_window(1, SavedPlacement::Tiled),
            saved_window(2, SavedPlacement::Tiled),
        ];
        let mut matcher = RestoreMatcher::new(&saved);
        let matched = matcher.try_match(&identity(2, 2)).expect("hard match");
        assert_eq!(matched.0, 1);
        assert_eq!(matcher.remaining_normal(), 1);
    }

    #[test]
    fn matcher_skips_ambiguous_fallback() {
        let mut first = saved_window(10, SavedPlacement::Tiled);
        first.pid = 99;
        first.title = "shared title".to_string();
        let mut second = saved_window(11, SavedPlacement::Tiled);
        second.pid = 100;
        second.title = "shared title".to_string();
        let saved = vec![first, second];

        let mut matcher = RestoreMatcher::new(&saved);
        // Same bundle/title/kind as both saved entries, but neither hard id
        // matches — the fallback is ambiguous and must be skipped.
        let mut current = identity(42, 42);
        current.title = "shared title".to_string();
        assert!(matcher.try_match(&current).is_none());
        assert_eq!(matcher.remaining_normal(), 2);
    }

    #[test]
    fn session_store_round_trips_and_rotates_backup() {
        let dir = std::env::temp_dir().join(format!(
            "tili-session-test-{}-{}",
            std::process::id(),
            unix_millis()
        ));
        let path = dir.join("session.json");
        let store = SessionStore::new(path.clone());

        let session = SavedSession::new(
            vec![],
            None,
            vec![],
            None,
            vec![SavedWorkspace {
                name: "main".to_string(),
                tree: TreeSnapshot::default(),
                focus: None,
                fullscreen: None,
                monitor: None,
            }],
            vec![saved_window(1, SavedPlacement::Tiled)],
        );
        store.save(&session).expect("save");
        assert!(
            matches!(store.load(), LoadOutcome::Loaded(loaded) if loaded.version == SESSION_VERSION)
        );

        store.save(&session).expect("second save");
        assert!(store.backup_path().exists());
        assert!(matches!(store.load(), LoadOutcome::Loaded(_)));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(store.backup_path());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn full_session_schema_round_trips_every_placement_kind() {
        let dir = std::env::temp_dir().join(format!(
            "tili-session-schema-{}-{}",
            std::process::id(),
            unix_millis()
        ));
        let store = SessionStore::new(dir.join("session.json"));
        let mut windows = vec![
            saved_window(1, SavedPlacement::Tiled),
            saved_window(
                2,
                SavedPlacement::Floating {
                    manual: Some(SavedFloatGeometry {
                        rel_x: 0.1,
                        rel_y: 0.2,
                        rel_w: 0.3,
                        rel_h: 0.4,
                    }),
                },
            ),
            saved_window(
                3,
                SavedPlacement::NativeFullscreen {
                    restore: SavedRestore::Tiled,
                },
            ),
            saved_window(
                4,
                SavedPlacement::Minimized {
                    restore: SavedRestore::Floating,
                },
            ),
            saved_window(
                5,
                SavedPlacement::HiddenApplication {
                    restore: SavedRestore::Tiled,
                },
            ),
            saved_window(6, SavedPlacement::Popup),
        ];
        windows[2].anchor = Some(TreeAnchor {
            before: Some(1),
            after: Some(2),
        });
        windows[3].special_manual = Some(SavedFloatGeometry {
            rel_x: 0.5,
            rel_y: 0.5,
            rel_w: 0.4,
            rel_h: 0.4,
        });
        windows[4].frame = Some(SavedRect {
            x: 1.0,
            y: 2.0,
            width: 300.0,
            height: 200.0,
        });

        let session = SavedSession::new(
            vec![SavedMonitor {
                id_hint: 1,
                frame: SavedRect {
                    x: 0.0,
                    y: 0.0,
                    width: 1000.0,
                    height: 800.0,
                },
                is_main: true,
            }],
            None,
            vec![],
            None,
            vec![SavedWorkspace {
                name: "main".to_string(),
                tree: TreeSnapshot {
                    root: Some(tili_tree::NodeSnapshot::Container {
                        layout: tili_tree::Layout::Accordion,
                        orientation: tili_tree::Orientation::Horizontal,
                        children: vec![
                            tili_tree::NodeSnapshot::Window(1),
                            tili_tree::NodeSnapshot::Window(2),
                        ],
                        weights: vec![1.0, 2.0],
                        mru: 1,
                    }),
                    collapsed_root_layout: Some(tili_tree::Layout::Accordion),
                },
                focus: Some(1),
                fullscreen: Some(1),
                monitor: None,
            }],
            windows,
        );
        store.save(&session).expect("save");
        match store.load() {
            LoadOutcome::Loaded(loaded) => assert_eq!(loaded, session),
            other => panic!("expected loaded session, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_primary_falls_back_to_backup() {
        let dir = std::env::temp_dir().join(format!(
            "tili-session-corrupt-{}-{}",
            std::process::id(),
            unix_millis()
        ));
        let path = dir.join("session.json");
        let store = SessionStore::new(path.clone());
        let session = SavedSession::new(vec![], None, vec![], None, vec![], vec![]);
        store.save(&session).expect("save");
        store.save(&session).expect("create backup");
        std::fs::write(&path, b"{ not json").expect("corrupt primary");
        assert!(matches!(store.load(), LoadOutcome::Loaded(_)));
        let _ = std::fs::remove_dir_all(dir);
    }
}
