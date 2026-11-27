//! `--live-session <file>`: the launcher opened a Nextcloud file for live editing. The window
//! joins the room the session file names over the native socket hub, with the file argument as
//! the room's base file, and picks up the launcher's fresh tickets from the same file.
//!
//! When the session cannot start (another format, no checksum, a relay elsewhere, a file that is
//! not the room's base, a presentation too large to edit live), the file opens for one person
//! and the window says that its changes are not shared. The ticket is never logged.

use std::path::Path;
use std::time::{Duration, Instant};

use craft_collab::Hub as _;
use deckcraft_live::desktop::{self, Change, LazyHub, NOT_SAVING_TEXT, NOT_SHARED_TEXT, RESTARTED_TEXT, SessionError, Watch};
use deckcraft_live::{BaseFile, DeckLive, LiveFail};
use deckcraft_ui_egui::SlideApp;
use deckcraft_ui_egui::live::{renewing, tr};

/// The window's live session from the launcher: the runtime its links run on, and the watch on
/// the session file.
pub struct DesktopLive {
    /// Dropped last (after the window), so the links end with it.
    _runtime: tokio::runtime::Runtime,
    watch: Watch,
    next_check: Instant,
}

/// Why a live start failed, for the person.
#[derive(Debug)]
pub struct NotStarted {
    pub why: String,
    /// The file is already open in the window (a presentation too large to edit live).
    pub opened: bool,
}

/// Joins the room `session` names with `file` as its base, and shows it in `app`.
///
/// # Errors
/// [`NotStarted`]: the file is then open for one person, or still to be opened.
pub fn start(app: &mut SlideApp, session: &Path, file: &str) -> Result<DesktopLive, NotStarted> {
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
            Ok(DesktopLive { _runtime: runtime, watch: Watch::new(session, &s), next_check: Instant::now() })
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

/// The room has no document service saving it right now: said, and kept in the status bar.
fn not_saving(app: &mut SlideApp) {
    let line = tr(NOT_SAVING_TEXT).to_owned();
    app.notify(line.clone());
    app.set_status(line);
}

/// Said when the file argument is not the room's base file.
const BASE_CHANGED: &str = "The file changed since the live session was opened. Reopen it from the launcher.";

/// The line said when the session did not start: `{why}`.
pub fn not_shared_line(why: &str) -> String {
    tr(NOT_SHARED_TEXT).replace("{why}", why)
}

impl DesktopLive {
    /// Once a frame: every two seconds, a fresh ticket in the session file goes to the hub (it
    /// opens a second link with it), and a session moved to another room ends this one.
    pub fn tick(&mut self, app: &mut SlideApp) {
        let now = Instant::now();
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
}
