//! Fork patch (76344243/wordcraft, branch `multi-window`): every document reopens where it was.
//!
//! When a window shows a document from a file, its place on screen (monitor, position, size) and
//! its zoom are remembered for that file, in `window-memory.json` next to the preferences. When
//! the file is opened again, in a new window or in this one, the window goes back there and the
//! zoom is restored. If that monitor is no longer connected, the window is centred on the main
//! monitor (the check `WindowGeometry::rescue_position` makes for the launch window). A file
//! opened for the first time keeps the window it got: the last one used, as before.
//!
//! What is remembered is written when the document leaves the window (another file opened in
//! it, the window closed or WordCraft quit). Each window is its own process; they share the file
//! by reading it fresh before every write, so one window never drops what another remembered.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wordcraft_ui_egui::{WordApp, window_geometry::WindowGeometry};

/// How many files are remembered; the ones used longest ago go first.
const MAX_ENTRIES: usize = 500;

/// How long after moving a window its monitor is checked: the move has to arrive first.
const RESCUE_DELAY: Duration = Duration::from_millis(600);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub window: WindowGeometry,
    /// The zoom (1.0 = 100 %), when no fit mode is on.
    pub zoom: f32,
    /// A fit mode ("pageWidth", "onePage", "multiplePages", "textWidth"), or empty.
    #[serde(default)]
    pub fit: String,
    /// When it was remembered (seconds since 1970), to drop the oldest first.
    #[serde(default)]
    pub at: u64,
}

impl Entry {
    /// The `view.zoom` parameter that brings this zoom back.
    fn zoom_param(&self) -> Option<Value> {
        if ["pageWidth", "onePage", "multiplePages", "textWidth"].contains(&self.fit.as_str()) {
            return Some(json!({"value": self.fit}));
        }
        let pct = (self.zoom * 100.0).round();
        (pct.is_finite() && (10.0..=500.0).contains(&pct)).then(|| json!({"value": pct}))
    }
}

pub type Memory = BTreeMap<String, Entry>;

/// The file, or `None` where preferences are off (tests, agents' runs).
fn file() -> Option<PathBuf> {
    if std::env::var_os("WORDCRAFT_NO_PREFS").is_some() {
        return None;
    }
    wordcraft_control_key::settings_dir().map(|d| d.join("window-memory.json"))
}

/// The key a file is remembered under: its full path with links resolved, so the same file opened
/// from the Finder, the command line or Recent is found again.
fn key(doc: &Path) -> String {
    std::fs::canonicalize(doc).unwrap_or_else(|_| doc.to_path_buf()).to_string_lossy().into_owned()
}

pub fn load_from(path: &Path) -> Memory {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Adds `entry` for `doc` to what `path` holds and writes it back atomically.
pub fn store_to(path: &Path, doc: &Path, mut entry: Entry) {
    let mut memory = load_from(path);
    entry.at = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    memory.insert(key(doc), entry);
    while memory.len() > MAX_ENTRIES {
        let Some(oldest) = memory.iter().min_by_key(|(_, e)| e.at).map(|(k, _)| k.clone()) else { break };
        memory.remove(&oldest);
    }
    let Ok(bytes) = serde_json::to_vec_pretty(&memory) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension(format!("json.{}", std::process::id()));
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The window for `doc` as remembered, for opening a new window there directly.
pub fn launch_geometry(doc: &str) -> Option<WindowGeometry> {
    load_from(&file()?).get(&key(Path::new(doc))).and_then(|e| e.window.sanitized())
}

/// What the window shows now: its file and how it shows it.
#[derive(Clone, Debug)]
struct Snapshot {
    doc: PathBuf,
    entry: Entry,
}

#[derive(Default)]
struct Tracker {
    /// The file shown on the last frame (`None`: no file, or not yet looked at).
    shown: Option<PathBuf>,
    /// The last frame's snapshot, written when the file leaves the window.
    last: Option<Snapshot>,
    /// A window moved to a remembered place, and when: its monitor is checked a little later.
    rescue: Option<(WindowGeometry, Instant)>,
}

thread_local! {
    static TRACKER: RefCell<Tracker> = RefCell::new(Tracker::default());
}

fn shown_file(app: &WordApp) -> Option<PathBuf> {
    app.has_document().then(|| app.session.path.clone()).flatten()
}

fn snapshot(app: &WordApp) -> Option<Snapshot> {
    let doc = shown_file(app)?;
    let window = app.ui.window?;
    Some(Snapshot { doc, entry: Entry { window, zoom: app.session.view.zoom, fit: app.session.view.fit.clone(), at: 0 } })
}

/// Every frame, after the window's geometry was tracked: notices a file arriving in or leaving
/// the window, remembers the one leaving, and puts the window and zoom where the arriving one was.
pub fn frame(app: &mut WordApp, ctx: &egui::Context) {
    let Some(store) = file() else { return };
    let shown = shown_file(app);
    TRACKER.with(|t| {
        let mut t = t.borrow_mut();
        if shown != t.shown {
            if let Some(last) = t.last.take() {
                store_to(&store, &last.doc, last.entry);
            }
            if let Some(doc) = &shown
                && let Some(entry) = load_from(&store).get(&key(doc)).cloned()
            {
                if let Some(g) = entry.window.sanitized() {
                    if g.maximized {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
                    } else {
                        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(g.width, g.height)));
                        if let Some([x, y]) = g.position {
                            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(x, y)));
                        }
                    }
                    t.rescue = Some((g, Instant::now()));
                }
                if let Some(param) = entry.zoom_param()
                    && let Err(e) = app.execute("view.zoom", param)
                {
                    log::warn!("window memory: zoom not restored: {e}");
                }
            }
            t.shown = shown;
        }
        // A remembered monitor that isn't there any more: centre the window on the main one.
        if let Some((g, since)) = t.rescue
            && since.elapsed() >= RESCUE_DELAY
        {
            t.rescue = None;
            if let Some(pos) = ctx.input(|i| g.rescue_position(i.viewport())) {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
            }
        } else if t.rescue.is_some() {
            ctx.request_repaint_after(RESCUE_DELAY);
        }
        if let Some(s) = snapshot(app) {
            t.last = Some(s);
        }
    });
}

/// When the window closes or WordCraft quits: remember the file shown now.
pub fn remember_now(app: &WordApp) {
    let Some(store) = file() else { return };
    if let Some(s) = snapshot(app) {
        store_to(&store, &s.doc, s.entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(x: f32) -> WindowGeometry {
        WindowGeometry { position: Some([x, 40.0]), width: 900.0, height: 700.0, maximized: false, monitor: Some([2560.0, 1440.0]) }
    }

    fn entry(x: f32, zoom: f32, fit: &str) -> Entry {
        Entry { window: geometry(x), zoom, fit: fit.into(), at: 0 }
    }

    #[test]
    fn entries_survive_other_windows_and_the_oldest_go_first() {
        let dir = std::env::temp_dir().join(format!("wc-window-memory-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = dir.join("window-memory.json");
        store_to(&store, Path::new("/docs/a.docx"), entry(10.0, 1.25, ""));
        // Another window (process) remembers its file: a.docx stays.
        store_to(&store, Path::new("/docs/b.docx"), entry(800.0, 1.0, "pageWidth"));
        let m = load_from(&store);
        assert_eq!(m["/docs/a.docx"].window.position, Some([10.0, 40.0]));
        assert_eq!(m["/docs/a.docx"].zoom, 1.25);
        assert_eq!(m["/docs/b.docx"].fit, "pageWidth");
        // Reopening a.docx somewhere else replaces its entry.
        store_to(&store, Path::new("/docs/a.docx"), entry(300.0, 0.8, ""));
        assert_eq!(load_from(&store)["/docs/a.docx"].window.position, Some([300.0, 40.0]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_zoom_comes_back_as_a_fit_mode_or_a_percentage() {
        assert_eq!(entry(0.0, 1.0, "onePage").zoom_param(), Some(json!({"value": "onePage"})));
        assert_eq!(entry(0.0, 1.5, "").zoom_param(), Some(json!({"value": 150.0})));
        assert_eq!(entry(0.0, f32::NAN, "").zoom_param(), None);
        assert_eq!(entry(0.0, 9.0, "").zoom_param(), None, "outside 10–500 %");
    }

    #[test]
    fn an_unreadable_file_is_an_empty_memory() {
        let dir = std::env::temp_dir().join(format!("wc-window-memory-bad-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let store = dir.join("window-memory.json");
        std::fs::write(&store, b"{ not json").unwrap();
        assert!(load_from(&store).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
