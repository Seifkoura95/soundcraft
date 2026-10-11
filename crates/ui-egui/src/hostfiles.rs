//! Files through the host instead of paths. A shell without a file system (the browser inside
//! SimpleHub) sets [`Services::open_async`], [`Services::inbox`] and [`Services::download`]: File ›
//! Open and the imports ask the host for a file (by purpose) and read the bytes that arrive in the
//! inbox; Save, Bounce and the exports hand the host the bytes they produce. Sessions travel as
//! single-file bundles ([`soundcraft_engine::bundle`]). With those services absent nothing here
//! runs and the path-based commands work as on the desktop.

use crate::{APP_NAME, Services, SoundApp};
use serde_json::{Value, json};
use soundcraft_engine::{bundle, io};
use std::sync::{Arc, Mutex};

/// A file the host gave the app: the purpose it was asked for, its name and its bytes.
#[derive(Debug, Clone)]
pub struct Arrival {
    pub purpose: String,
    pub name: String,
    pub bytes: Vec<u8>,
}

/// Where the host puts files the app asked for (drained every frame).
pub type Inbox = Arc<Mutex<Vec<Arrival>>>;

/// Purposes passed to [`Services::open_async`] so the host can offer the right files.
pub const PURPOSE_OPEN: &str = "open";
pub const PURPOSE_IMPORT: &str = "import";
pub const PURPOSE_MIDI: &str = "midi";
pub const PURPOSE_VIDEO: &str = "video";

/// What the host is asked for, per purpose (lower-case extensions), for hosts that filter.
pub const EXTENSIONS: &[(&str, &[&str])] = &[
    (PURPOSE_OPEN, &["scraft.zip", "scraft", "zip"]),
    (PURPOSE_IMPORT, &["wav", "wave", "bwf", "aif", "aiff", "aifc", "flac", "mp3", "ogg", "oga", "m4a", "aac", "mp4", "caf", "mkv", "webm"]),
    (PURPOSE_MIDI, &["mid", "midi", "smf"]),
    (PURPOSE_VIDEO, &["mp4", "m4v", "mov"]),
];

/// True when files go through the host.
pub fn active(services: &Services) -> bool {
    services.open_async.is_some() && services.download.is_some()
}

/// Menu-style invocation of `id` (a menu item, shortcut or the command palette). True when
/// handled: the host was asked for a file, the command ran and its file went to the host, or the
/// command needs a folder and is not available here.
pub fn request(app: &mut SoundApp, id: &str) -> bool {
    if !active(&app.services) {
        return false;
    }
    match id {
        "session.open" => ask(app, PURPOSE_OPEN),
        "file.import_audio" => ask(app, PURPOSE_IMPORT),
        "file.import_midi" => ask(app, PURPOSE_MIDI),
        "file.import_video" => ask(app, PURPOSE_VIDEO),
        "session.save"
        | "session.save_as"
        | "session.save_copy"
        | "session.save_template"
        | "file.export_selected_tracks_as_session"
        | "file.export_midi"
        | "file.export_session_text"
        | "file.export_sibelius"
        | "file.send_to_sibelius"
        | "file.print_score" => {
            let _ = app.run(id, json!({}));
            true
        }
        // Bounce keeps its dialog (format, bit depth, normalise); `run` routes the result.
        "file.bounce_mix" => false,
        _ if crate::dialogs::takes_path(id) => {
            let label = soundcraft_engine::find_command(id).map_or(id, |c| c.label.trim_end_matches("..."));
            app.ui.status = format!("{label} needs a folder on disk and is not available in {APP_NAME}.");
            true
        }
        _ => false,
    }
}

fn ask(app: &mut SoundApp, purpose: &str) -> bool {
    if let Some(open) = &mut app.services.open_async {
        open(purpose);
    }
    true
}

/// `SoundApp::run` of a file command without a file system: Some when handled here. Opens and
/// imports called with no file (`{}`) ask the host; saves and exports produce their file and hand
/// it over, named after the session (or after `path`'s file name when one is given).
pub fn run(app: &mut SoundApp, id: &str, p: &Value) -> Option<Result<Value, String>> {
    if !active(&app.services) {
        return None;
    }
    let given = p.get("path").and_then(Value::as_str).map(file_name).filter(|n| !n.is_empty());
    let session = io::sanitize_name(&app.engine.session().name);
    let r = match id {
        "session.open" | "file.import_audio" | "file.import_midi" | "file.import_video" => {
            if given.is_some() || p.get("paths").is_some() {
                return None;
            }
            let purpose = match id {
                "session.open" => PURPOSE_OPEN,
                "file.import_midi" => PURPOSE_MIDI,
                "file.import_video" => PURPOSE_VIDEO,
                _ => PURPOSE_IMPORT,
            };
            ask(app, purpose);
            return Some(Ok(json!({"asked": purpose})));
        }
        "session.save" | "session.save_as" | "session.save_copy" => {
            let name = given.map(|n| format!("{}.{}", bundle::stem(&n), bundle::EXTENSION)).unwrap_or_else(|| bundle::file_name(&app.engine));
            let keep = (id == "session.save_copy").then(|| app.engine.session().name.clone());
            let r = bundle::save(&mut app.engine, &name).map(|b| (name, b));
            if let Some(name) = keep {
                // A copy leaves the open session as it was, name included.
                app.engine.session_mut().name = name;
                app.engine.mark_clean();
            }
            r
        }
        "session.save_template" => {
            let name = given.unwrap_or_else(|| format!("{session} Template.{}", soundcraft_model::SESSION_EXTENSION));
            io::template_of(app.engine.session())
                .to_json()
                .map(|t| (name, t.into_bytes()))
                .map_err(|e| soundcraft_engine::EngineError::Io(e.to_string()))
        }
        "file.export_selected_tracks_as_session" => {
            let keep = match soundcraft_engine::cmd::tracks_required(&app.engine, id, p) {
                Ok(k) => k,
                Err(e) => return Some(fail(app, id, e)),
            };
            let name = given
                .map(|n| format!("{}.{}", bundle::stem(&n), bundle::EXTENSION))
                .unwrap_or_else(|| format!("{session} Tracks.{}", bundle::EXTENSION));
            bundle::of(bundle::tracks_only(app.engine.session(), &keep)).map(|b| (name, b))
        }
        "file.bounce_mix" => {
            let ext = match p.get("format").and_then(Value::as_str) {
                Some("aiff" | "aif") => "aif",
                Some("flac") => "flac",
                Some(_) => "wav",
                None => given.as_deref().and_then(|n| n.rsplit_once('.').map(|(_, e)| e)).unwrap_or("wav"),
            }
            .to_string();
            let name = given.unwrap_or_else(|| format!("{session} Bounce.{ext}"));
            soundcraft_engine::cmd::bounce_output(&mut app.engine, p).map(|(bytes, _)| (name, bytes))
        }
        "file.export_midi" => io::export_midi(&app.engine).map(|b| (given.unwrap_or_else(|| format!("{session}.mid")), b)),
        "file.export_session_text" => {
            Ok((given.unwrap_or_else(|| format!("{session}.txt")), soundcraft_engine::inspect::session_text(&app.engine).into_bytes()))
        }
        "file.export_sibelius" | "file.send_to_sibelius" | "file.print_score" => {
            soundcraft_engine::cmd::score_output(&mut app.engine, p, id, id == "file.print_score")
                .map(|(text, name, _)| (given.unwrap_or(name), text.into_bytes()))
        }
        _ => return None,
    };
    Some(match r {
        Ok((name, bytes)) => {
            let len = bytes.len();
            if let Some(download) = &mut app.services.download {
                download(&name, &bytes);
            }
            app.ui.status = format!("Saved {name}");
            Ok(json!({"name": name, "bytes": len}))
        }
        Err(e) => fail(app, id, e),
    })
}

fn fail(app: &mut SoundApp, id: &str, e: soundcraft_engine::EngineError) -> Result<Value, String> {
    app.ui.status = e.to_string();
    log::warn!("{id}: {e}");
    Err(e.to_string())
}

/// The files the host delivered since the last frame: open the session, or import at the
/// insertion point (one undo step each).
pub fn drain(app: &mut SoundApp) {
    let Some(inbox) = app.services.inbox.clone() else { return };
    let arrived: Vec<Arrival> = std::mem::take(&mut *inbox.lock().unwrap_or_else(|e| e.into_inner()));
    for a in arrived {
        let at = app.engine.session().edit.selection.start.max(0);
        let r = match a.purpose.as_str() {
            PURPOSE_OPEN => bundle::open_bytes(&mut app.engine, &a.name, &a.bytes).map(|missing| {
                if missing.is_empty() { format!("Opened {}", a.name) } else { format!("Opened {} ({} media files missing)", a.name, missing.len()) }
            }),
            PURPOSE_MIDI => undoable(app, "Import MIDI", |e| io::import_midi_bytes(e, &a.bytes, at, true).map(|_| format!("Imported {}", a.name))),
            PURPOSE_VIDEO => {
                undoable(app, "Import Video", |e| io::import_video_bytes(e, &a.name, a.bytes.clone(), at).map(|_| format!("Imported {}", a.name)))
            }
            _ => undoable(app, "Import Audio", |e| {
                io::import_audio_bytes(e, &a.name, &a.bytes, None, None, at).map(|_| format!("Imported {}", a.name))
            }),
        };
        match r {
            Ok(m) => app.ui.status = m,
            Err(e) => {
                app.ui.status = e.to_string();
                log::warn!("{}: {e}", a.name);
            }
        }
    }
}

/// Run `f` on the engine as one undo step called `label`; the document is left as it was when
/// `f` fails.
fn undoable<T>(
    app: &mut SoundApp,
    label: &str,
    f: impl FnOnce(&mut soundcraft_engine::Engine) -> soundcraft_engine::Result<T>,
) -> soundcraft_engine::Result<T> {
    let before = app.engine.session_arc();
    let dirty = app.engine.is_dirty();
    match f(&mut app.engine) {
        Ok(v) => {
            app.engine.push_undo(label, before);
            Ok(v)
        }
        Err(e) => {
            if !Arc::ptr_eq(&before, &app.engine.session_arc()) {
                *app.engine.session_mut() = soundcraft_model::Session::clone(&before);
            }
            if !dirty {
                app.engine.mark_clean();
            }
            Err(e)
        }
    }
}

/// The last component of a path, as the host's file name.
fn file_name(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).trim().to_string()
}
