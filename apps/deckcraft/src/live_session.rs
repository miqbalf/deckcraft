//! `--live-session <file>`: the launcher opened a Nextcloud file for live editing. The window
//! joins the room the session file names over the native socket hub, with the file argument as
//! the room's base file, and picks up the launcher's fresh tickets from the same file.
//!
//! When the window cannot go live, it falls back (brief section 2b): it writes
//! `<session file>.fallback` (the launcher then takes the check-out flow and uploads the saves),
//! opens the working copy for one person under its own path, so Save writes it, and says so. It
//! falls back before it shows the file when the session cannot start (another format, no
//! checksum, a relay elsewhere, a file that is not the room's base, a presentation too large to
//! edit live), and later when the relay refuses the link, or has not joined it within
//! [`JOIN_WITHIN`], before it was ever live. A link that was live once and drops is not a
//! fallback: it reconnects. The ticket is never logged.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use craft_collab::Hub as _;
use craft_live::LiveStatus;
use deckcraft_live::desktop::{
    self, Change, LazyHub, NOT_SAVING_TEXT, NOT_SHARED_TEXT, RELAY_REFUSED_TEXT, RELAY_UNREACHABLE_TEXT, RESTARTED_TEXT, SessionError, Watch,
};
use deckcraft_live::{BaseFile, DeckLive, LiveFail};
use deckcraft_ui_egui::SlideApp;
use deckcraft_ui_egui::live::{renewing, tr};

/// How long the relay may take to join the first link before the window falls back.
pub const JOIN_WITHIN: Duration = Duration::from_secs(30);

/// Said (as the fallback reason) when the file argument is not the room's base file.
const BASE_CHANGED: &str = "The file changed since the live session was opened.";

/// The window's live session from the launcher: the runtime its links run on, the watch on the
/// session file, and what a fallback needs.
pub struct DesktopLive {
    /// Dropped last (after the window), so the links end with it.
    _runtime: tokio::runtime::Runtime,
    watch: Watch,
    next_check: Instant,
    session: PathBuf,
    /// The working copy: the live presentation's file once the window falls back.
    file: String,
    started: Instant,
    /// The link was live once: from then on a drop reconnects and never falls back.
    joined: bool,
    /// The window fell back: nothing more to do.
    done: bool,
}

/// Opens the launcher's file in `app`: live when the session in `session` starts, otherwise
/// for one person after the fallback signal. Removes the base file from `files` once it is open,
/// so the caller opens only the others.
pub fn open(app: &mut SlideApp, session: &str, files: &mut Vec<String>) -> Option<DesktopLive> {
    let session = Path::new(session);
    let Some(base) = files.first().cloned() else {
        log::warn!("--live-session without a file: opened nothing live");
        signal(session, tr(SessionError::Missing("file").to_string().as_str()));
        return None;
    };
    match start(app, session, &base) {
        Ok(live) => {
            files.remove(0);
            Some(live)
        }
        Err(NotStarted { why, opened }) => {
            log::warn!("live editing did not start: {why}");
            // Before the file shows, so the launcher uploads every save.
            signal(session, &why);
            files.remove(0);
            if !opened {
                for (f, e) in app.open_from_system(std::slice::from_ref(&base)) {
                    log::error!("{f}: {e}");
                }
            }
            take_over_file(app, None, &base);
            tell(app, &not_shared_line(&why));
            None
        }
    }
}

/// Writes the fallback signal; a failure is logged (the launcher then stays in live mode, and the
/// window has said that live editing is not available).
fn signal(session: &Path, reason: &str) {
    if let Err(e) = desktop::write_fallback(session, reason) {
        log::error!("the fallback signal could not be written: {e}");
    }
}

/// Why a live start failed, for the person.
#[derive(Debug)]
struct NotStarted {
    why: String,
    /// The file is already open in the window (a presentation too large to edit live).
    opened: bool,
}

/// Joins the room `session` names with `file` as its base, and shows it in `app`.
fn start(app: &mut SlideApp, session: &Path, file: &str) -> Result<DesktopLive, NotStarted> {
    let not = |e: &dyn std::fmt::Display| NotStarted { why: tr(&e.to_string()).to_owned(), opened: false };
    let s = desktop::read(session).map_err(|e| not(&e))?;
    let sha = s.base_sha256.ok_or(SessionError::NoBaseHash).map_err(|e| not(&e))?;
    let relay = s.relay_endpoint().map_err(|e| not(&e))?;
    let bytes = std::fs::read(file).map_err(|e| not(&e))?;
    let runtime =
        tokio::runtime::Builder::new_multi_thread().worker_threads(1).thread_name("deckcraft-live").enable_all().build().map_err(|e| not(&e))?;
    let schema = deckcraft_live::schema();
    let hub = LazyHub::connect(runtime.handle(), &relay, &s.ticket, None, &schema);
    // A renewed ticket opens a second link that takes over once it has joined (as the web app's).
    let handle = runtime.handle().clone();
    let hub = renewing(hub, move |r: &craft_collab::Reopen<'_>| Ok(LazyHub::connect(&handle, &relay, r.ticket, r.applied, &schema)));
    let name = Path::new(file).file_name().map_or_else(|| file.to_owned(), |n| n.to_string_lossy().into_owned());
    let base = BaseFile { bytes: &bytes, name: &name, expected_sha256: Some(sha) };
    let uid = s.uid().unwrap_or_else(|| "user".to_owned());
    match DeckLive::start(hub, &s.doc(), &base, s.access(), &uid, &mut app.session) {
        Ok(live) => {
            app.live_saved_by_room = true;
            app.start_live(live);
            log::info!("live: joining room {} for file {:?}", s.room, s.file_id);
            if !s.saver {
                not_saving(app);
            }
            let now = Instant::now();
            Ok(DesktopLive {
                _runtime: runtime,
                watch: Watch::new(session, &s),
                next_check: now,
                session: session.to_owned(),
                file: file.to_owned(),
                started: now,
                joined: false,
                done: false,
            })
        }
        Err(e) => {
            let opened = e == LiveFail::TooLarge;
            let why = match e {
                LiveFail::BaseMismatch => tr(BASE_CHANGED).to_owned(),
                other => tr(&other.to_string()).to_owned(),
            };
            Err(NotStarted { why, opened })
        }
    }
}

/// The presentation of `uid` (or the active one) becomes the working copy's, saved as it is: Save
/// then writes the working copy, which the launcher uploads, and the window no longer counts on
/// the room to save it.
fn take_over_file(app: &mut SlideApp, uid: Option<u64>, file: &str) {
    app.live_saved_by_room = false;
    let uid = uid.or_else(|| app.session.active().map(|d| d.uid));
    if let Some(st) = uid.and_then(|u| app.session.document_mut(u)) {
        st.path = Some(file.to_owned());
        st.saved_doc = Arc::clone(&st.doc);
    }
}

fn tell(app: &mut SlideApp, line: &str) {
    app.notify(line.to_owned());
    app.set_status(line.to_owned());
}

/// The room has no document service saving it right now: said, and kept in the status bar.
fn not_saving(app: &mut SlideApp) {
    tell(app, tr(NOT_SAVING_TEXT));
}

/// The line said when the window fell back: `{why}`.
fn not_shared_line(why: &str) -> String {
    tr(NOT_SHARED_TEXT).replace("{why}", why)
}

impl DesktopLive {
    /// Once a frame: falls back when the relay refused the first link or has not joined it in
    /// time; every two seconds, a fresh ticket in the session file goes to the hub (it opens a
    /// second link with it), and a session moved to another room ends this one.
    pub fn tick(&mut self, app: &mut SlideApp) {
        if self.done {
            return;
        }
        let now = Instant::now();
        if !self.joined && self.first_join(app, now) {
            return;
        }
        if now < self.next_check {
            return;
        }
        self.next_check = now + Duration::from_millis(desktop::WATCH_EVERY_MS);
        let saver = self.watch.saver();
        let change = self.watch.check();
        if saver && !self.watch.saver() {
            not_saving(app);
        }
        match change {
            Some(Change::Ticket(ticket)) => {
                if let Some(lu) = app.live.as_mut() {
                    lu.live_mut().core_mut().hub_mut().set_token(ticket);
                }
            }
            Some(Change::Restarted) => app.end_live_for(tr(RESTARTED_TEXT)),
            None => {}
        }
    }

    /// Until the first join: notes it, or falls back. True when the window fell back.
    fn first_join(&mut self, app: &mut SlideApp, now: Instant) -> bool {
        let Some(lu) = app.live.as_ref() else {
            // The window closed the live presentation before it ever joined: nothing to fall
            // back for.
            self.done = true;
            return true;
        };
        let live = lu.live();
        let reason = match live.status() {
            LiveStatus::Live { .. } => {
                self.joined = true;
                return false;
            }
            LiveStatus::ReadOnly if live.core().hub().connected() => {
                self.joined = true;
                return false;
            }
            LiveStatus::Refused(_) | LiveStatus::Broken(_) => RELAY_REFUSED_TEXT,
            _ if now.duration_since(self.started) >= JOIN_WITHIN => RELAY_UNREACHABLE_TEXT,
            _ => return false,
        };
        self.fall_back(app, tr(reason));
        true
    }

    /// The window leaves the room that never joined and keeps the presentation for one person,
    /// as the working copy's (nothing was edited: edits wait for the join).
    fn fall_back(&mut self, app: &mut SlideApp, reason: &str) {
        self.done = true;
        log::warn!("live editing did not start: {reason}");
        signal(&self.session, reason);
        let uid = app.live.as_ref().map(|lu| lu.live().doc());
        app.end_live();
        take_over_file(app, uid, &self.file);
        tell(app, &not_shared_line(reason));
    }

    #[cfg(test)]
    fn started_long_ago(&mut self) {
        self.started = Instant::now().checked_sub(JOIN_WITHIN + Duration::from_secs(1)).unwrap_or(self.started);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deckcraft_engine::Session;
    use deckcraft_ui_egui::Services;

    /// A folder of its own with a working copy (a real `.pptx`) and a session file.
    fn setup(name: &str, sha: Option<[u8; 32]>) -> (PathBuf, String, String) {
        let dir = std::env::temp_dir().join(format!("deck-live-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let s = Session::with_new();
        let bytes = deckcraft_engine::cmd::file::save_bytes(&s.active().unwrap().doc, "pptx").unwrap();
        let file = dir.join("Deck.pptx");
        std::fs::write(&file, &bytes).unwrap();
        let sha = sha.unwrap_or_else(|| deckcraft_live::seed::base_sha256(&bytes));
        let hex: String = sha.iter().map(|b| format!("{b:02x}")).collect();
        // Nothing listens on port 9 here: the relay is never reached.
        let session = serde_json::json!({"v": 1, "server": "http://127.0.0.1:9", "ticket": "e30.sig", "relayUrl": "/craft",
            "room": "inst:7:g", "base": {"sha256": hex}});
        let path = dir.join("s.json");
        std::fs::write(&path, serde_json::to_vec(&session).unwrap()).unwrap();
        (dir, path.to_string_lossy().into_owned(), file.to_string_lossy().into_owned())
    }

    fn reason(session: &str) -> Option<String> {
        let bytes = std::fs::read(desktop::fallback_path(Path::new(session))).ok()?;
        let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        v["reason"].as_str().map(str::to_owned)
    }

    /// The working copy is the open presentation's file, saved, and Save writes it.
    fn edits_the_working_copy(app: &SlideApp, file: &str) -> bool {
        app.live.is_none() && !app.live_saved_by_room && app.session.active().is_some_and(|d| d.path.as_deref() == Some(file) && !d.is_dirty())
    }

    #[test]
    fn a_base_that_is_not_the_rooms_falls_back_before_the_file_shows() {
        let (dir, session, file) = setup("mismatch", Some([7; 32]));
        let mut app = SlideApp::new(Session::new(), Services::default());
        let mut files = vec![file.clone(), "other.pptx".to_owned()];
        assert!(open(&mut app, &session, &mut files).is_none());
        assert_eq!(files, ["other.pptx".to_owned()], "the base is opened here, the rest by the caller");
        assert!(reason(&session).is_some_and(|r| !r.is_empty()));
        assert!(edits_the_working_copy(&app, &file));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_relay_that_never_joins_falls_back_and_hands_the_file_over() {
        let (dir, session, file) = setup("unreachable", None);
        let mut app = SlideApp::new(Session::new(), Services::default());
        let mut files = vec![file.clone()];
        let mut live = open(&mut app, &session, &mut files).expect("the session starts");
        assert!(app.live.is_some() && app.live_saved_by_room);
        live.tick(&mut app);
        assert_eq!(reason(&session), None, "no fallback while the relay may still answer");
        live.started_long_ago();
        live.tick(&mut app);
        assert!(reason(&session).is_some_and(|r| !r.is_empty()));
        assert!(edits_the_working_copy(&app, &file));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
