//! Fork patch (76344243/wordcraft, branch `window-memory`): every document opens where it was,
//! and View › Set Open-Standard sets where new ones open.
//!
//! WordCraft gives each document a window of its own ([`crate::windows`]), cascaded from the
//! last. With this patch a window that starts showing a file goes to the place (monitor,
//! position, size) and zoom that file had when its window or zoom was last changed. A document
//! without its own place (a new one, a file opened for the first time, a file merely opened and
//! closed before) gets the Open-Standard, if one was set. A remembered monitor that is gone: the
//! window is centred on the main one. A second window on the same document (View › New Window)
//! keeps WordCraft's cascade.
//!
//! A file gets its own entry only when its window or zoom was changed while it was shown
//! (compared with how it looked once settled), written when it leaves the window (another file
//! opened in it, the window closed, WordCraft quit). Entries live in `window-memory.json`
//! ([`crate::Services::window_memory`]), the standard under its own key; the 500 most recent files
//! are kept, keyed by the resolved path.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::WordApp;
use crate::window_geometry::{RawWindowState, WindowGeometry};

/// How many files are remembered; the ones used longest ago go first.
const MAX_ENTRIES: usize = 500;

/// The key the Open-Standard is kept under, beside the files (never a real path).
const STANDARD: &str = "<open-standard>";

/// How long after moving a window its monitor is checked, and twice that, how long a window
/// settles before what it shows counts as "as opened".
const SETTLE: Duration = Duration::from_millis(600);

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
    /// Remembered because the user changed the window or zoom. Entries without it (the first
    /// version remembered every file opened) are ignored.
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

/// The key a file is remembered under: its full path with links resolved, so the same file opened
/// from the Finder, the Open dialog or Recent is found again.
fn file_key(doc: &Path) -> String {
    std::fs::canonicalize(doc).unwrap_or_else(|_| doc.to_path_buf()).to_string_lossy().into_owned()
}

pub fn load_from(path: &Path) -> Memory {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Adds `entry` for `doc` to what `path` holds and writes it back atomically. Each write reads
/// the file fresh, so what other WordCraft processes wrote stays.
pub fn store_to(path: &Path, doc: &Path, entry: Entry) -> Result<(), String> {
    store_key(path, file_key(doc), entry)
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

/// One window, as window memory follows it.
#[derive(Default)]
struct Tracker {
    /// The document it showed on its last frame (`None` before its first).
    document: Option<u64>,
    /// Its window as last tracked.
    window: Option<WindowGeometry>,
    /// The file it shows and how, as of its last frame.
    last: Option<(PathBuf, Entry)>,
    /// How it looked once settled after its document arrived; remembered only if it differs.
    baseline: Option<Entry>,
    baseline_at: Option<Instant>,
    /// Moved to a remembered place, and when: its monitor is checked a little later.
    rescue: Option<(WindowGeometry, Instant)>,
}

#[derive(Default)]
struct State {
    windows: HashMap<u64, Tracker>,
    /// A window was placed from window memory: the host's start-up shrink-to-fit
    /// ([`WindowGeometry::fit_on_monitor`]) must leave it alone ([`placed_any`]).
    placed: bool,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// Whether a window was placed from window memory. The desktop host then skips its start-up
/// shrink-to-fit, which took a near-full-height window down to the monitor minus two system bars
/// and centred it.
pub fn placed_any() -> bool {
    STATE.with(|s| s.borrow().placed)
}

/// Writes the snapshot of the document leaving window `t`, if the user changed how it was shown.
fn leave(store: &Path, t: &mut Tracker) {
    let (Some((doc, entry)), Some(base)) = (t.last.take(), t.baseline.take()) else { return };
    if changed(&entry, &base)
        && let Err(e) = store_to(store, &doc, Entry { changed: true, ..entry })
    {
        log::warn!("window memory: not saved: {e}");
    }
}

/// A frame of a window whose document stayed: the delayed monitor check, the snapshot, the
/// baseline once the window settled.
fn frame_after(ctx: &egui::Context, t: &mut Tracker, raw: &RawWindowState, shown: Option<PathBuf>, zoom: f32, fit: String) {
    if let Some((g, since)) = t.rescue
        && since.elapsed() >= SETTLE
    {
        t.rescue = None;
        if let Some(pos) = g.rescue_position(raw) {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
        }
    } else if t.rescue.is_some() {
        ctx.request_repaint_after(SETTLE);
    }
    t.last = shown.zip(t.window).map(|(doc, window)| (doc, Entry { window, zoom, fit, at: 0, changed: false }));
    if let Some(at) = t.baseline_at {
        if Instant::now() >= at {
            t.baseline_at = None;
            t.baseline = t.last.as_ref().map(|(_, e)| e.clone());
        } else {
            ctx.request_repaint_after(at.saturating_duration_since(Instant::now()));
        }
    }
}

/// Moves and sizes the drawn window to `entry`'s, and checks its monitor a little later.
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

impl WordApp {
    /// Every frame of window `key`, after it was drawn, in its own viewport: notices a document
    /// arriving in it or leaving it, remembers the one leaving, and puts the window and zoom where
    /// the arriving one was (or at the Open-Standard).
    pub(crate) fn window_memory_frame(&mut self, ctx: &egui::Context, key: u64) {
        let Some(store) = self.services.window_memory.clone() else { return };
        let raw = ctx.input(|i| RawWindowState::from_viewport(Some(i.viewport()), Some(i.viewport_rect())));
        let document = self.shown_document;
        let shown = self.has_document().then(|| self.session.path.clone()).flatten();
        let second_window = self.other_window_on_document().is_some();
        let (zoom, fit) = (self.session.view.zoom, self.session.view.fit.clone());
        let mut apply_zoom = None;
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            let mut placed = false;
            let t = s.windows.entry(key).or_default();
            t.window = WindowGeometry::track(t.window, &raw);
            if t.document != Some(document) {
                leave(&store, t);
                let memory = load_from(&store);
                let own = shown.as_ref().and_then(|doc| memory.get(&file_key(doc)).cloned()).filter(|e| e.changed);
                let entry = own.or_else(|| memory.get(STANDARD).cloned());
                if let Some(entry) = entry {
                    // A second window on a document keeps WordCraft's cascade.
                    if !second_window {
                        place(ctx, t, &entry);
                    }
                    apply_zoom = entry.zoom_param();
                }
                t.document = Some(document);
                t.baseline = None;
                t.baseline_at = Some(Instant::now() + SETTLE * 2);
                placed = t.rescue.is_some();
            } else {
                frame_after(ctx, t, &raw, shown, zoom, fit);
            }
            s.placed |= placed;
        });
        if let Some(param) = apply_zoom
            && let Err(e) = self.execute("view.zoom", param)
        {
            log::warn!("window memory: zoom not restored: {e}");
        }
    }

    /// After the windows were drawn: the windows that closed since remember their documents.
    pub(crate) fn window_memory_closed(&mut self) {
        let Some(store) = self.services.window_memory.clone() else { return };
        let open = self.windows.keys();
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            let gone: Vec<u64> = s.windows.keys().copied().filter(|k| !open.contains(k)).collect();
            for k in gone {
                if let Some(mut t) = s.windows.remove(&k) {
                    leave(&store, &mut t);
                }
            }
        });
    }

    /// When WordCraft quits: every window remembers its document.
    pub fn window_memory_quit(&self) {
        let Some(store) = self.services.window_memory.clone() else { return };
        STATE.with(|s| {
            for t in s.borrow_mut().windows.values_mut() {
                leave(&store, t);
            }
        });
    }

    /// View › Set Open-Standard: the drawn window's place and zoom become the standard.
    pub(crate) fn set_open_standard(&mut self) {
        let Some(store) = self.services.window_memory.clone() else { return };
        let key = self.windows.shown();
        let window = STATE.with(|s| s.borrow().windows.get(&key).and_then(|t| t.window)).and_then(WindowGeometry::sanitized);
        let msg = match window {
            None => "Open-Standard: the window's size isn't known yet".to_string(),
            Some(window) => {
                let (zoom, fit) = (self.session.view.zoom, self.session.view.fit.clone());
                match store_key(&store, STANDARD.into(), Entry { window, zoom, fit: fit.clone(), at: 0, changed: false }) {
                    Ok(()) => {
                        let shown = if fit.is_empty() { format!("{:.0} %", zoom * 100.0) } else { crate::tl!(fit_label(&fit)).to_string() };
                        format!("Open-Standard: {:.0} × {:.0}, {shown}", window.width, window.height)
                    }
                    Err(e) => format!("Open-Standard: {e}"),
                }
            }
        };
        self.status(msg);
    }
}

fn fit_label(fit: &str) -> &'static str {
    match fit {
        "pageWidth" => "Page Width",
        "onePage" => "One Page",
        "multiplePages" => "Multiple Pages",
        _ => "Text Width",
    }
}

/// Whether `path` holds an Open-Standard.
pub fn has_standard(path: &Path) -> bool {
    load_from(path).contains_key(STANDARD)
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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wc-window-memory-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("window-memory.json")
    }

    #[test]
    fn entries_survive_other_processes_and_are_replaced() {
        let store = scratch("entries");
        store_to(&store, Path::new("/docs/a.docx"), entry(10.0, 1.25, "")).unwrap();
        store_to(&store, Path::new("/docs/b.docx"), entry(800.0, 1.0, "pageWidth")).unwrap();
        let m = load_from(&store);
        assert_eq!(m["/docs/a.docx"].window.position, Some([10.0, 40.0]));
        assert_eq!(m["/docs/b.docx"].fit, "pageWidth");
        store_to(&store, Path::new("/docs/a.docx"), entry(300.0, 0.8, "")).unwrap();
        assert_eq!(load_from(&store)["/docs/a.docx"].window.position, Some([300.0, 40.0]));
        let _ = std::fs::remove_dir_all(store.parent().unwrap());
    }

    #[test]
    fn the_open_standard_is_never_dropped() {
        let store = scratch("standard");
        store_key(&store, STANDARD.into(), entry(50.0, 1.1, "")).unwrap();
        for i in 0..MAX_ENTRIES + 5 {
            store_to(&store, Path::new(&format!("/docs/{i}.docx")), entry(0.0, 1.0, "")).unwrap();
        }
        assert_eq!(load_from(&store).len(), MAX_ENTRIES);
        assert!(has_standard(&store));
        let _ = std::fs::remove_dir_all(store.parent().unwrap());
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
        let store = scratch("bad");
        std::fs::create_dir_all(store.parent().unwrap()).unwrap();
        std::fs::write(&store, b"{ not json").unwrap();
        assert!(load_from(&store).is_empty());
        let _ = std::fs::remove_dir_all(store.parent().unwrap());
    }
}
