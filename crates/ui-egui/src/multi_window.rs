//! Fork patch (76344243/wordcraft, branch `multi-window`): any number of WordCraft windows.
//!
//! Upstream WordCraft is one document per window and one window per app, so New, Open, Recent
//! and a Finder double-click replace the document that is showing (after asking about unsaved
//! changes), and View › New Window does nothing. With this patch, when the window already holds a
//! document, those open in a new WordCraft window (a new process) instead, as Word does. An
//! untouched blank document is still replaced in place, so the first document opened after
//! launch doesn't leave an empty window behind.
//!
//! The host provides the launcher ([`crate::Services::new_window`]); without it (web, tests,
//! scripts) everything behaves as upstream.

use crate::WordApp;
use serde_json::{Value, json};

/// Whether this window holds a document worth keeping: one that was opened or saved, changed,
/// or with any text in it. A closed window ("no document") and an untouched blank one don't.
pub(crate) fn in_use(app: &WordApp) -> bool {
    if !app.has_document() {
        return false;
    }
    let s = &app.session;
    s.path.is_some() || s.dirty || s.can_undo() || !s.doc.plain_text(wordcraft_doc::StoryRef::Body).trim().is_empty() || s.doc.paragraph_count() > 1
}

/// The document a user command would open in a new window, if it should go to one: `Some(None)`
/// for a new blank document, `Some(Some(path))` for a file. Templates, passwords (the retry of an
/// open that asked for one) and files handed over as bytes stay in this window.
fn target(app: &WordApp, id: &str, params: &Value) -> Option<Option<String>> {
    match id {
        "file.new" => {
            let blank = params.get("template").and_then(Value::as_str).is_none_or(|t| t == "blank");
            blank.then_some(None)
        }
        "file.open" => {
            if params.get("data").is_some() || params.get("password").is_some() {
                return None;
            }
            let path = params.get("path").and_then(Value::as_str)?;
            // Opening the file this window already shows reopens it here, as before.
            if app.session.path.as_deref() == Some(std::path::Path::new(path)) {
                return None;
            }
            Some(Some(path.to_string()))
        }
        _ => None,
    }
}

/// Called by [`WordApp::run`] before a user command replaces the document: start a new window
/// for it instead when this one is in use. `None` lets the command run here.
pub(crate) fn redirect(app: &mut WordApp, id: &str, params: &Value) -> Option<Result<Value, String>> {
    let path = target(app, id, params)?;
    if app.services.new_window.is_none() || !in_use(app) {
        return None;
    }
    Some(open(app, path.as_deref()))
}

/// Start a new WordCraft window, with a file or a new blank document.
pub(crate) fn open(app: &mut WordApp, path: Option<&str>) -> Result<Value, String> {
    let Some(launch) = app.services.new_window.as_ref() else {
        return Err("New windows aren't available here".into());
    };
    match launch(path) {
        Ok(()) => {
            if app.ui.backstage {
                app.ui.backstage = false;
            }
            Ok(json!({"newWindow": true, "path": path}))
        }
        Err(e) => {
            let msg = crate::tl!("Couldn't open a new window: {error}").replace("{error}", &e);
            app.status(msg.clone());
            Err(msg)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Services;
    use std::sync::{Arc, Mutex};
    use wordcraft_engine::Session;

    fn app_with_launcher() -> (WordApp, Arc<Mutex<Vec<Option<String>>>>) {
        let launched = Arc::new(Mutex::new(Vec::new()));
        let l = launched.clone();
        let services = Services {
            new_window: Some(Box::new(move |p: Option<&str>| {
                l.lock().unwrap().push(p.map(str::to_string));
                Ok(())
            })),
            ..Default::default()
        };
        (WordApp::new(Session::new(wordcraft_doc::Document::new()), services), launched)
    }

    #[test]
    fn a_blank_window_is_replaced_in_place() {
        let (mut app, launched) = app_with_launcher();
        assert!(!in_use(&app));
        app.run("file.new", json!({})).unwrap();
        assert!(launched.lock().unwrap().is_empty());
    }

    #[test]
    fn new_and_open_go_to_a_new_window_once_there_is_text() {
        let (mut app, launched) = app_with_launcher();
        app.run("text.insert", json!({"text": "Hallo"})).unwrap();
        assert!(in_use(&app));
        let r = app.run("file.new", json!({})).unwrap();
        assert_eq!(r["newWindow"], json!(true));
        app.run("file.open", json!({"path": "/tmp/brief.docx"})).unwrap();
        assert_eq!(*launched.lock().unwrap(), vec![None, Some("/tmp/brief.docx".to_string())]);
        // The text is still here: nothing was replaced and nothing asked to save.
        assert_eq!(app.session.doc.plain_text(wordcraft_doc::StoryRef::Body).trim(), "Hallo");
        assert!(app.dialog.is_none());
    }

    #[test]
    fn templates_and_scripts_stay_in_this_window() {
        let (mut app, launched) = app_with_launcher();
        app.run("text.insert", json!({"text": "Hallo"})).unwrap();
        assert!(target(&app, "file.new", &json!({"template": "letter"})).is_none());
        // `execute` (agents, scripts) never goes through `run`'s redirect.
        app.execute("file.new", json!({})).unwrap();
        assert!(launched.lock().unwrap().is_empty());
    }

    #[test]
    fn without_a_launcher_nothing_changes() {
        let mut app = WordApp::new(Session::new(wordcraft_doc::Document::new()), Services::default());
        app.run("text.insert", json!({"text": "Hallo"})).unwrap();
        assert!(redirect(&mut app, "file.new", &json!({})).is_none());
    }
}
