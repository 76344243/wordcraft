//! Fork patch (76344243/wordcraft, branch `multi-window`): every document reopens where it was.
//!
//! When a window shows a document from a file, its place on screen (monitor, position, size) and
//! its zoom are remembered for that file, in `window-memory.json` next to the preferences. When
//! the file is opened again, in a new window or in this one, the window goes back there and the
//! zoom is restored. If that monitor is no longer connected, the window is centred on the main
//! monitor (the check `WindowGeometry::rescue_position` makes for the launch window). A file
//! opened for the first time keeps the window it got: the last one used, as before.
//!
//! View › Set Open-Standard makes the window as it is now (monitor, position, size) and its zoom
//! the standard: new documents, new windows and files without their own entry open that way.
//! Files that already have their own entry keep it.
//!
//! A file only gets its own entry when its window or zoom was changed while it was shown (moved,
//! resized, zoomed): a file merely opened and closed again keeps following the Open-Standard.
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

/// The key the Open-Standard is kept under, beside the files (never a real path).
const STANDARD: &str = "<open-standard>";

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
    /// Remembered because the user changed the window or zoom. Entries from the first version,
    /// which remembered every file opened, lack it and are ignored.
    #[serde(default)]
    pub changed: bool,
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
pub fn store_to(path: &Path, doc: &Path, entry: Entry) -> Result<(), String> {
    store_key(path, key(doc), entry)
}

fn store_key(path: &Path, k: String, mut entry: Entry) -> Result<(), String> {
    let mut memory = load_from(path);
    entry.at = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    memory.insert(k, entry);
    while memory.len() > MAX_ENTRIES {
        let Some(oldest) = memory.iter().filter(|(k, _)| *k != STANDARD).min_by_key(|(_, e)| e.at).map(|(k, _)| k.clone()) else { break };
        memory.remove(&oldest);
    }
    let bytes = serde_json::to_vec_pretty(&memory).map_err(|e| e.to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension(format!("json.{}", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

/// The Open-Standard in `path`, if one was set.
#[cfg(test)]
fn standard_in(path: &Path) -> Option<Entry> {
    load_from(path).remove(STANDARD)
}

/// View › Set Open-Standard (the `Services::set_open_default` hook).
pub fn set_standard(window: Option<WindowGeometry>, zoom: f32, fit: &str) -> Result<(), String> {
    let store = file().ok_or("preferences are off")?;
    let window = window.and_then(WindowGeometry::sanitized).ok_or("the window's size isn't known yet")?;
    store_key(&store, STANDARD.into(), Entry { window, zoom, fit: fit.into(), at: 0, changed: false })
}

/// Where a new window opens: where `doc` was last, else at the Open-Standard. `None`: neither is
/// known, the window opens as before (the last window used).
pub fn launch_geometry(doc: Option<&str>) -> Option<WindowGeometry> {
    let mut memory = load_from(&file()?);
    let own = doc.and_then(|d| memory.remove(&key(Path::new(d)))).filter(|e| e.changed);
    own.or_else(|| memory.remove(STANDARD)).and_then(|e| e.window.sanitized())
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
    /// The document shown on the last frame (`None` before the first frame): a new id is a
    /// document arriving (opened or new), not the same one saved under another name.
    document: Option<u64>,
    /// The last frame's snapshot, written when the file leaves the window.
    last: Option<Snapshot>,
    /// A window moved to a remembered place, and when: its monitor is checked a little later.
    rescue: Option<(WindowGeometry, Instant)>,
    /// How the document was shown once its window had settled after it arrived; it is
    /// remembered only if it differs from this when it leaves.
    baseline: Option<Entry>,
    /// When to take the baseline.
    baseline_at: Option<Instant>,
}

/// Whether the user changed the window or zoom: more than a couple of points of movement or
/// size (the system may round), another zoom or fit mode.
fn changed(now: &Entry, base: &Entry) -> bool {
    let (a, b) = (&now.window, &base.window);
    let far = |x: f32, y: f32| (x - y).abs() > 2.0;
    let moved = match (a.position, b.position) {
        (Some([x1, y1]), Some([x2, y2])) => far(x1, x2) || far(y1, y2),
        (p, q) => p.is_some() != q.is_some(),
    };
    moved
        || far(a.width, b.width)
        || far(a.height, b.height)
        || a.maximized != b.maximized
        || (now.zoom - base.zoom).abs() > 0.005
        || now.fit != base.fit
}

/// Writes the snapshot of the document leaving, if the user changed how it was shown.
fn leave(store: &Path, t: &mut Tracker) {
    let (Some(last), Some(base)) = (t.last.take(), t.baseline.take()) else { return };
    if changed(&last.entry, &base)
        && let Err(e) = store_to(store, &last.doc, Entry { changed: true, ..last.entry })
    {
        log::warn!("window memory: not saved: {e}");
    }
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
    Some(Snapshot { doc, entry: Entry { window, zoom: app.session.view.zoom, fit: app.session.view.fit.clone(), at: 0, changed: false } })
}

/// Every frame, after the window's geometry was tracked: notices a file arriving in or leaving
/// the window, remembers the one leaving, and puts the window and zoom where the arriving one was.
pub fn frame(app: &mut WordApp, ctx: &egui::Context) {
    let Some(store) = file() else { return };
    let shown = shown_file(app);
    let document = app.session.document_id();
    TRACKER.with(|t| {
        let mut t = t.borrow_mut();
        let arrived = t.document != Some(document);
        if arrived {
            // The document leaving is remembered under its file, if the user changed its window.
            leave(&store, &mut t);
        }
        // Saved under another name: the same document, and the snapshot below takes the new file.
        if arrived {
            // Its own place if the file has one; otherwise the Open-Standard, if set.
            let memory = load_from(&store);
            let own = shown.as_ref().and_then(|doc| memory.get(&key(doc)).cloned()).filter(|e| e.changed);
            let entry = own.or_else(|| memory.get(STANDARD).cloned());
            if let Some(entry) = entry {
                // Also on the first frame, where the window already opened there
                // (`launch_geometry`): this way its monitor is checked as well.
                place(ctx, &mut t, &entry);
                if let Some(param) = entry.zoom_param()
                    && let Err(e) = app.execute("view.zoom", param)
                {
                    log::warn!("window memory: zoom not restored: {e}");
                }
            }
            t.document = Some(document);
            t.baseline = None;
            t.baseline_at = Some(Instant::now() + RESCUE_DELAY * 2);
        }
        t.shown = shown;
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
        t.last = snapshot(app);
        if let Some(at) = t.baseline_at {
            if Instant::now() >= at {
                t.baseline_at = None;
                t.baseline = t.last.as_ref().map(|s| s.entry.clone());
            } else {
                ctx.request_repaint_after(at.saturating_duration_since(Instant::now()));
            }
        }
    });
}

/// Moves and sizes the window to `entry`'s, and checks its monitor a little later.
fn place(ctx: &egui::Context, t: &mut Tracker, entry: &Entry) {
    let Some(g) = entry.window.sanitized() else { return };
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

/// When the window closes or WordCraft quits: remember the file shown now, if its window or zoom
/// was changed.
pub fn remember_now(app: &WordApp) {
    let Some(store) = file() else { return };
    TRACKER.with(|t| {
        let mut t = t.borrow_mut();
        t.last = snapshot(app);
        leave(&store, &mut t);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(x: f32) -> WindowGeometry {
        WindowGeometry { position: Some([x, 40.0]), width: 900.0, height: 700.0, maximized: false, monitor: Some([2560.0, 1440.0]) }
    }

    fn entry(x: f32, zoom: f32, fit: &str) -> Entry {
        Entry { window: geometry(x), zoom, fit: fit.into(), at: 0, changed: true }
    }

    #[test]
    fn entries_survive_other_windows_and_the_oldest_go_first() {
        let dir = std::env::temp_dir().join(format!("wc-window-memory-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = dir.join("window-memory.json");
        store_to(&store, Path::new("/docs/a.docx"), entry(10.0, 1.25, "")).unwrap();
        // Another window (process) remembers its file: a.docx stays.
        store_to(&store, Path::new("/docs/b.docx"), entry(800.0, 1.0, "pageWidth")).unwrap();
        let m = load_from(&store);
        assert_eq!(m["/docs/a.docx"].window.position, Some([10.0, 40.0]));
        assert_eq!(m["/docs/a.docx"].zoom, 1.25);
        assert_eq!(m["/docs/b.docx"].fit, "pageWidth");
        // Reopening a.docx somewhere else replaces its entry.
        store_to(&store, Path::new("/docs/a.docx"), entry(300.0, 0.8, "")).unwrap();
        assert_eq!(load_from(&store)["/docs/a.docx"].window.position, Some([300.0, 40.0]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_open_standard_sits_beside_the_files_and_is_never_dropped() {
        let dir = std::env::temp_dir().join(format!("wc-window-memory-std-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = dir.join("window-memory.json");
        store_key(&store, STANDARD.into(), entry(50.0, 1.1, "")).unwrap();
        for i in 0..MAX_ENTRIES + 5 {
            store_to(&store, Path::new(&format!("/docs/{i}.docx")), entry(0.0, 1.0, "")).unwrap();
        }
        let m = load_from(&store);
        assert_eq!(m.len(), MAX_ENTRIES);
        assert_eq!(standard_in(&store).map(|e| e.zoom), Some(1.1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_window_or_zoom_the_user_changed_counts() {
        let base = entry(100.0, 1.2, "");
        assert!(!changed(&entry(101.5, 1.2, ""), &base), "a point of rounding");
        assert!(changed(&entry(160.0, 1.2, ""), &base), "moved");
        assert!(changed(&entry(100.0, 1.5, ""), &base), "zoomed");
        assert!(changed(&entry(100.0, 1.2, "pageWidth"), &base), "fit mode");
        let mut bigger = base.clone();
        bigger.window.height += 40.0;
        assert!(changed(&bigger, &base), "resized");
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
