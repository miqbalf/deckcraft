//! The host bridge, for pages that embed DeckCraft and keep its files (a Nextcloud app, a personal
//! file server). The messages and the two transports are `craft-web-host`'s
//! (storytold/craft-libs), the same contract as the other Craft apps' bridges, with the
//! `deckcraft:` prefix. This module is DeckCraft's part of it.
//!
//! - `?host=parent` (an `<iframe>`) talks to the same-origin parent over `postMessage`;
//!   `data-host="page"` on the canvas (the page's own document, no iframe) uses DOM
//!   `CustomEvent`s on the canvas instead. `?host=parent` wins when both are present. With no
//!   parent, or a parent on another origin, there is no bridge: the normal app with downloads.
//! - `deckcraft:ready` (to the host): sent once the app is running and listening; send a file now. DeckCraft
//!   reads bytes, so it says `file: false` and a host sends `bytes`.
//! - `deckcraft:open` (from the host): `{ name, bytes }`, bytes an `ArrayBuffer` or
//!   `Uint8Array`, name a bare file name. It opens like a file picked with File › Open on the
//!   desktop: a presentation sent with its name (.pptx, .potx, .ppsx, .deckcraft) has that name
//!   as its file, so Save goes back to it; an outline (.txt, .md, .rtf), a file without a name and
//!   a damaged file open untitled.
//! - `deckcraft:save` (to the host): `{ name, saveAs, bytes }` (with the bytes as a transferred `ArrayBuffer` over `postMessage`) for
//!   every file DeckCraft would otherwise download. The host stores it and reports failures
//!   itself. `saveAs` says whether the file becomes the presentation's file:
//!   - Save: `false`. It writes the opened file under its own name and format (a .pptx stays a
//!     .pptx), as the desktop app does. A name the parent never sent (an untitled presentation, an
//!     outline, a file sent without a name, a repaired copy of a damaged file, a file the user
//!     picked or dropped) is a new file: `true`, as for Save As, named `<title>.deckcraft` as
//!     Save suggests on the desktop.
//!   - Save As (File › Save As as .pptx or .deckcraft, Ctrl+Shift+S): `true`. The presentation is
//!     now that file; the next Save sends its name with `false`.
//!   - Export (PDF, pictures, outline, GIF, handouts, a Save As type that isn't a presentation)
//!     and Print: `false`. They are copies; the presentation's file stays the same.
//! - `deckcraft:host` with `saves: true` (from the host): the host stores saves and answers each.
//!   `deckcraft:save` then carries an `id` (for exports and Print too), the presentation counts as
//!   saved only on `deckcraft:saved { id, name? }` (a `name` is its file from then on: a conflict
//!   copy, Save As), and on `deckcraft:save-failed { id, error }` it stays unsaved with "Couldn't
//!   save to Nextcloud: {error}". Without `saves` nothing changes: a file handed over counts as
//!   saved at once.
//! - `deckcraft:fonts { fonts: [{name, url}] }` (from the host): each same-origin font file is
//!   fetched and installed like a font the user added; the missing-font notice waits for them.
//! - `deckcraft:dirty` whenever unsaved changes appear or go away, `deckcraft:failed` when the
//!   app couldn't start or refused a file of 2 GiB or larger.
//!
//! # Live editing (`craft-live/1`, `?host=parent` only)
//!
//! The parent page may instead send the unprefixed `live` message: `{ticket, relayUrl, room,
//! fileUrl, fileName, proto}` (`craft-web-host`'s `live` module checks every field and refuses
//! another `proto`). DeckCraft posts the unprefixed `{type:"ready"}` once it listens.
//!
//! - The first `live` fetches `fileUrl` (the room's pinned base file, Ruling D2) with the user's
//!   cookies, only from this page's own origin over HTTP(S) ([`crate::live_ticket::same_origin`]);
//!   a 404 is retried briefly (the file may still be written). The URL must carry the base's
//!   `sha256`, and bytes that hash differently are refused ([`crate::live_ticket::checked_base`]),
//!   so there is no join.
//! - It then connects to the relay on the page's own host only ([`crate::live_ticket::same_host`])
//!   over the page's `socket.io-client` (`craft-collab`'s `WebHub`), announcing the
//!   `deckcraft/1_<build>` schema, and starts `DeckLive` on the file with the URL's hash and the
//!   ticket's access (a ticket without the update bit joins read-only), on the page's clock. If
//!   the fetch, the connect or the start fails, the window says why and the next `live` tries
//!   again ([`crate::live_ticket::Joining`]).
//! - A later `live` for the same room is the page's ticket refresh: it goes to the hub
//!   (`Hub::set_token`), a `craft_collab::Handover` that opens a second link with it and switches
//!   once that one has joined, so the old ticket's expiry drops nothing. One for another room, or
//!   one that gives less access, ends the session.
//! - `live-state` `{state, reason, savedAt, peers}` goes to the parent on every change of state
//!   and at most once a second otherwise.
//! - While live (and after a `live` message that failed), the presentation is not saved from this
//!   window: the document service saves the room. Presentation saves are refused and `dirty`
//!   stays false; exports still go out. The one exception is the copy of edits the room could not
//!   merge: it goes to the page at once as `deckcraft:save {name, saveAs: true, bytes}` under its
//!   own name (Ruling D18).
//!
//! Tickets are never logged.
//!
//! In host mode the app starts with an empty presentation instead of the sample, which gives way to
//! the first file the host sends; the host guards leaving the page, and `?author=` names the user
//! for comments.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use craft_live::LiveStatus;
use craft_web_host::Mode;
use craft_web_host::live::{LiveMessage, LiveState, LiveStatus as HostStatus, Peer as HostPeer};
use craft_web_host::web::{FileData, Host, Message, fetch_font};
use deckcraft_live::{BaseFile, DeckLive, LiveFail};
use deckcraft_ui_egui::{HostAnswer, HostSaves, Services, SlideApp};
use wasm_bindgen::{JsCast as _, JsValue};

use crate::live_ticket::{Joining, Next};

/// The app id in the host messages (`deckcraft:ready`).
const APP: &str = "deckcraft";

/// The name `craft-web-host` gives an open request that carries none (it cleans a host's name and
/// falls back to this): no name is no file, so the presentation opens untitled.
const NAMELESS: &str = "document";

/// The largest file a host may send. File › Open on the desktop refuses anything over 2 GiB.
/// On wasm32 a `Vec` cannot hold 2 GiB (`isize::MAX` is one byte less), so a file of exactly
/// 2 GiB is refused too, before its bytes are copied.
const MAX_OPEN_BYTES: f64 = 2_147_483_648.0;

/// The longest user name taken from `?author=` (the preferences' own limit).
const MAX_AUTHOR_CHARS: usize = 256;

/// What the host's `fonts` message leaves for the next frame.
enum FontEvent {
    /// Fonts are being fetched (true), or all arrived or failed (false).
    Coming(bool),
    /// A fetched font file.
    File(Vec<u8>),
}

#[derive(Clone)]
pub struct Bridge {
    host: Rc<Host>,
    /// Whether the host stores saves and answers each, and its answers (shared with the app).
    saves: HostSaves,
    fonts: Rc<RefCell<Vec<FontEvent>>>,
    /// Files the host sent (the name, if it sent one), waiting for the next frame.
    opened: Rc<RefCell<Vec<(Option<String>, Vec<u8>)>>>,
    /// The window edits a room: its document service saves the presentation, this window does not.
    live_on: Rc<Cell<bool>>,
    /// The page sent a `live` message: this window is a room's, joined or not. Never cleared, so a
    /// failed join cannot turn into a save over the room's file.
    live_asked: Rc<Cell<bool>>,
}

impl Bridge {
    /// The bridge for this page, or `None` when DeckCraft isn't hosted: no `?host=parent` and no
    /// `data-host="page"` on the canvas, an unframed `?host=parent` page (it stays the normal app
    /// with downloads), or a parent on another origin.
    pub fn detect(canvas: &web_sys::HtmlCanvasElement) -> Option<Self> {
        let host = Host::connect(APP, canvas)?;
        // An opaque origin (a file:// page in some browsers) can't be a postMessage target.
        if host.mode() == craft_web_host::Mode::Parent && web_sys::window()?.location().origin().ok()? == "null" {
            return None;
        }
        Some(Self {
            host: Rc::new(host),
            saves: HostSaves::default(),
            fonts: Rc::default(),
            opened: Rc::default(),
            live_on: Rc::default(),
            live_asked: Rc::default(),
        })
    }

    /// Hand saved and exported files to the host instead of downloading them, and hear the files
    /// it sends.
    pub fn connect(&self, services: &mut Services, ctx: egui::Context) {
        self.listen(ctx);
        // The app and the listener share one answer queue.
        services.host_saves = self.saves.clone();
        let bridge = self.clone();
        services.host_save =
            Some(Box::new(move |name: &str, bytes: &[u8], save_as: bool, id: Option<&str>| bridge.post_save(name, bytes, save_as, id)));
        // The copy of unmerged edits is a new file, also while live (Ruling D18).
        let bridge = self.clone();
        services.save_copy = Some(Box::new(move |name: &str, bytes: &[u8]| bridge.post_copy(name, bytes)));
        // Exports and Print are copies: the host answers them too, so a failure is shown.
        let bridge = self.clone();
        services.download = Some(Box::new(move |name: &str, bytes: &[u8]| {
            let id = bridge.saves.copy_id();
            if let Err(e) = bridge.post_save(name, bytes, false, id.as_deref()) {
                log::error!("couldn't hand {name} to the page: {e}");
                if let Some(id) = id {
                    // No answer will come: say so now.
                    bridge.saves.answer(HostAnswer::Failed { id, error: e });
                }
            }
        }));
    }

    /// `?author=`: the host's name for the user signs new comments.
    pub fn set_author(app: &mut SlideApp, author: &str) {
        let author: String = author.trim().chars().take(MAX_AUTHOR_CHARS).collect();
        if !author.is_empty() {
            app.session.prefs.author = author;
        }
    }

    pub fn post_ready(&self) {
        let _ = self.host.post_ready(env!("CARGO_PKG_VERSION"), false);
        // A live host waits for the unprefixed `ready` before it sends `live`.
        if self.host.mode() == Mode::Parent {
            let _ = self.host.post_live_ready();
        }
    }

    /// The app couldn't start (no WebGPU or WebGL2, for example): let the host say so.
    pub fn post_failed(&self, error: &str) {
        let _ = self.host.post_failed(error, None);
    }

    /// Before each frame: fonts the host's files may need go in first.
    pub fn install_fonts(&self, app: &mut SlideApp) {
        let events = std::mem::take(&mut *self.fonts.borrow_mut());
        for event in events {
            match event {
                FontEvent::Coming(coming) => app.host_fonts_coming(coming),
                FontEvent::File(bytes) => {
                    if app.install_host_font(bytes) == 0 {
                        log::warn!("a host font was not a font DeckCraft can read, or was already there");
                    }
                }
            }
        }
    }

    /// Before each frame: open the files the host sent.
    pub fn open_files(&self, app: &mut SlideApp) {
        let opened = std::mem::take(&mut *self.opened.borrow_mut());
        for (name, bytes) in opened {
            app.open_from_host(name.as_deref(), &bytes);
        }
    }

    /// After each frame: tell the host when unsaved changes appear or go away (the crate does not
    /// repeat a state it already reported). A live room is saved by its document service, so
    /// only unconfirmed edits and a copy not saved yet count while live.
    pub fn report_dirty(&self, app: &SlideApp) {
        let docs_dirty = app.session.documents().iter().any(|d| d.is_dirty());
        self.host.post_dirty(crate::live_ticket::page_dirty(self.live_on.get(), docs_dirty, app.live_unconfirmed(), app.copies_unsaved()));
    }

    fn post_save(&self, path: &str, bytes: &[u8], save_as: bool, id: Option<&str>) -> Result<(), String> {
        let name = crate::live_ticket::bare_name(path);
        if let Some(why) = crate::live_ticket::save_refusal(self.live_on.get(), self.live_asked.get(), &name) {
            return Err(deckcraft_ui_egui::live::tr(why).to_owned());
        }
        self.host.post_save(&name, bytes, id, save_as)
    }

    /// The copy of unmerged edits, as a new file (`saveAs: true`). A host that answers saves gets
    /// an id for it, so the window keeps the copy until the page says it saved (or failed); a
    /// message that could not be posted is an error at once, with no answer queued.
    fn post_copy(&self, path: &str, bytes: &[u8]) -> Result<Option<String>, String> {
        let id = self.saves.copy_id();
        let (name, save_as) = crate::live_ticket::copy_save(path);
        match self.host.post_save(&name, bytes, id.as_deref(), save_as) {
            Ok(()) => Ok(id),
            Err(e) => {
                if let Some(id) = id {
                    self.saves.finish_copy(&id);
                }
                Err(e)
            }
        }
    }

    /// Hear `deckcraft:open` and queue the file for the next frame.
    fn listen(&self, ctx: egui::Context) {
        let opened = self.opened.clone();
        let host = self.host.clone();
        let saves = self.saves.clone();
        let fonts = self.fonts.clone();
        self.host.listen(move |message| {
            let (name, data) = match message {
                Message::Open { name, data } => (name, data),
                Message::Host { saves: true, .. } => return saves.set_confirms(),
                Message::Saved { id, name } => {
                    saves.answer(HostAnswer::Saved { id, name });
                    return ctx.request_repaint();
                }
                Message::SaveFailed { id, error } => {
                    saves.answer(HostAnswer::Failed { id, error });
                    return ctx.request_repaint();
                }
                Message::Fonts { fonts: list } => return fetch_fonts(list, fonts.clone(), ctx.clone()),
                _ => return,
            };
            if data.size() >= MAX_OPEN_BYTES {
                log::warn!("host file {name} is 2 GiB or larger; not opened");
                let _ = host.post_failed(&format!("{name} is 2 GiB or larger, which DeckCraft's web build cannot open"), None);
                return;
            }
            let FileData::Bytes(array) = data else {
                log::warn!("host file {name} came as a File; DeckCraft reads bytes only; not opened");
                return;
            };
            // No name is no file: the presentation opens untitled, never under an invented name.
            let name = (name != NAMELESS).then_some(name);
            opened.borrow_mut().push((name, array.to_vec()));
            ctx.request_repaint();
        });
    }
}

/// Fetch each font a host offered (one at a time, same-origin only, size-capped by the bridge) and
/// leave its bytes for the next frame; a failure is logged and skipped.
fn fetch_fonts(list: Vec<craft_web_host::FontRef>, events: Rc<RefCell<Vec<FontEvent>>>, ctx: egui::Context) {
    if list.is_empty() {
        return;
    }
    events.borrow_mut().push(FontEvent::Coming(true));
    ctx.request_repaint();
    wasm_bindgen_futures::spawn_local(async move {
        for font in list {
            match fetch_font(&font).await {
                Ok(bytes) => events.borrow_mut().push(FontEvent::File(bytes)),
                Err(e) => log::warn!("host font {} not fetched: {e}", font.name),
            }
            ctx.request_repaint();
        }
        events.borrow_mut().push(FontEvent::Coming(false));
        ctx.request_repaint();
    });
}

// ---- live editing ----

/// Told when the page speaks a newer live protocol than this build.
const NEWER_TEXT: &str = "This file is being edited with a newer DeckCraft. Update to join.";
/// Told when the page's `live` message was refused.
const UNUSABLE_TEXT: &str = "Live editing could not start. Saving is off in this window; reopen the file.";
/// Told when the file did not open for live editing: `{error}`.
const NOT_OPENED_TEXT: &str = "Couldn't open the file for live editing: {error}";
/// Told when the base file did not arrive in time; the local document stays usable.
const BASE_TIMEOUT: &str = "The file did not arrive within 30 seconds.";

/// How often a base file that is not there yet is asked for again, and how long apart.
const BASE_RETRIES: u32 = 4;
const BASE_RETRY_MS: i32 = 500;
/// The whole base fetch gives up after this.
const BASE_TIMEOUT_MS: i32 = 30_000;
/// The largest base file taken (the host open limit).
const MAX_BASE_BYTES: u64 = (2 << 30) - 1;
/// The least time between two `live-state` posts that only differ in the peers or the save.
const REPORT_EVERY_MS: f64 = 1000.0;

impl Bridge {
    /// Hear the parent's `live` messages: the first fetches the file and joins, later ones are
    /// ticket refreshes. They wait in `events` for the next frame ([`Live::frame`]).
    pub fn listen_live(&self, events: LiveEvents, joining: Rc<RefCell<Joining>>, ctx: egui::Context) {
        let asked = self.live_asked.clone();
        self.host.listen_live(move |message| match message {
            Err(e) => {
                // The error never quotes the message, so no ticket reaches the log.
                log::warn!("live message refused: {e}");
                let event = match crate::live_ticket::note_refused(&asked, &e) {
                    crate::live_ticket::Refusal::Newer => LiveEvent::Newer,
                    crate::live_ticket::Refusal::Unusable => LiveEvent::Unusable,
                };
                events.borrow_mut().push(event);
                ctx.request_repaint();
            }
            Ok(message) => {
                asked.set(true);
                let access = crate::live_ticket::ticket_access(message.ticket());
                let next = joining.borrow_mut().message(&message.room, access);
                match next {
                    Next::Restarted | Next::AccessDropped => {
                        let reason = next.end_reason().unwrap_or(crate::live_ticket::RESTARTED);
                        log::warn!("the live session ended: {reason}");
                        events.borrow_mut().push(LiveEvent::End(reason));
                        ctx.request_repaint();
                    }
                    Next::Ignore => {}
                    Next::Refresh => {
                        events.borrow_mut().push(LiveEvent::Ticket(message.ticket().to_owned()));
                        ctx.request_repaint();
                    }
                    Next::Open => {
                        let (events, ctx, joining) = (events.clone(), ctx.clone(), joining.clone());
                        wasm_bindgen_futures::spawn_local(async move {
                            let fetched = match fetch_file(&message.file_url).await {
                                Ok(bytes) => {
                                    crate::live_ticket::checked_base(&message.file_url, &bytes).map(|hash| (bytes, hash)).map_err(str::to_owned)
                                }
                                Err(e) => Err(e),
                            };
                            let event = match fetched {
                                Ok((bytes, hash)) => LiveEvent::Open(Box::new(message), bytes, hash),
                                Err(e) => {
                                    // The page's next `live` (its refresh) tries again.
                                    joining.borrow_mut().failed();
                                    LiveEvent::Failed(e)
                                }
                            };
                            events.borrow_mut().push(event);
                            ctx.request_repaint();
                        });
                    }
                }
            }
        });
    }
}

/// What the parent's `live` messages brought, for the next frame.
pub enum LiveEvent {
    /// The base file, fetched and checked against its hash: join with this message's ticket.
    Open(Box<LiveMessage>, Vec<u8>, [u8; 32]),
    /// A later `live` for the room: a fresh ticket.
    Ticket(String),
    /// The file could not be fetched or checked.
    Failed(String),
    /// The room speaks a newer protocol.
    Newer,
    /// The page's `live` message could not be used; saving is off.
    Unusable,
    /// The session ended (another room, less access), for this reason.
    End(&'static str),
}

pub type LiveEvents = Rc<RefCell<Vec<LiveEvent>>>;

/// The web app's side of a live session: when it joins, a ticket that came during the join, and
/// what was last reported to the page.
#[derive(Default)]
pub struct Live {
    joining: Rc<RefCell<Joining>>,
    /// A fresh ticket that arrived while the file was fetched, for the hub once joined.
    pending: Option<String>,
    reported: Option<Reported>,
    /// The newest save marker seen, and when this window saw it (page milliseconds).
    saved: Option<(String, f64)>,
    /// When this window last asked the page to renew its ticket (`live-renew`, page ms).
    renew_asked: Option<f64>,
}

struct Reported {
    state: LiveState,
    at: f64,
}

impl Live {
    /// The join state the `live` listener shares ([`Bridge::listen_live`]).
    pub fn joining(&self) -> Rc<RefCell<Joining>> {
        Rc::clone(&self.joining)
    }

    /// Before each frame: act on what arrived since the last frame.
    pub fn before(&mut self, app: &mut SlideApp, events: &LiveEvents, bridge: &Bridge) {
        let arrived = std::mem::take(&mut *events.borrow_mut());
        for event in arrived {
            match event {
                LiveEvent::Newer => tell(app, NEWER_TEXT),
                LiveEvent::Unusable => tell(app, UNUSABLE_TEXT),
                // Edits the room never confirmed come back as the copy (Review C1).
                LiveEvent::End(reason) => app.end_live_for(reason),
                LiveEvent::Failed(e) => {
                    log::error!("live file not fetched: {e}");
                    // A ticket that came during the fetch is older than the next join's.
                    self.pending = None;
                    not_opened(app, &e);
                }
                LiveEvent::Open(message, bytes, hash) => self.open(app, bridge, &message, &bytes, hash),
                // The hub uses it for its next connect, and starts again with it after the relay
                // refused an expired one.
                LiveEvent::Ticket(ticket) => {
                    if !refresh_ticket(app, &ticket) {
                        self.pending = Some(ticket);
                    }
                }
            }
        }
    }

    /// After each frame: hand a copy of unmerged edits to the page as soon as there is one
    /// (Ruling D18; not again after the page failed to save one), and tell the page how the
    /// session stands.
    pub fn after(&mut self, app: &mut SlideApp, bridge: &Bridge) {
        app.hand_over_copies();
        self.report(app, bridge);
        self.ask_renewal(app);
    }

    /// While live, ask the page every [`RENEW_ASK_MS`](crate::live_ticket::RENEW_ASK_MS) to
    /// renew the ticket if it is due (`{type:"live-renew"}`). The page renews from a timer, which
    /// runs minutes late while this window keeps the main thread busy; these frames still run, and
    /// a renewed ticket arrives as a `live` message (DC1). The page ignores the ask otherwise.
    fn ask_renewal(&mut self, app: &SlideApp) {
        if app.live.is_none() {
            self.renew_asked = None;
            return;
        }
        let now = js_sys::Date::now();
        if !crate::live_ticket::renew_ask_due(now, self.renew_asked) {
            return;
        }
        self.renew_asked = Some(now);
        let Some(window) = web_sys::window() else { return };
        let (Ok(Some(parent)), Ok(origin)) = (window.parent(), window.location().origin()) else { return };
        let ask = js_sys::Object::new();
        if js_sys::Reflect::set(&ask, &JsValue::from_str("type"), &JsValue::from_str("live-renew")).is_ok()
            && let Err(e) = parent.post_message(&ask, &origin)
        {
            log::warn!("live-renew not delivered: {e:?}");
        }
    }

    fn open(&mut self, app: &mut SlideApp, bridge: &Bridge, message: &LiveMessage, bytes: &[u8], hash: [u8; 32]) {
        let access = crate::live_ticket::ticket_access(message.ticket());
        let uid = crate::live_ticket::ticket_uid(message.ticket()).unwrap_or_else(|| "user".to_owned());
        let started = relay_on_page_host(&message.relay_url)
            .and_then(|()| {
                craft_collab::web::WebHub::connect_as(&message.relay_url, message.ticket(), None, Some(&deckcraft_live::schema()))
                    .map_err(|e| e.to_string())
            })
            .map_err(|e| (e, None))
            .and_then(|hub| {
                // A renewed ticket opens a second link that takes over once it has joined, so the
                // old ticket's expiry never drops the link (LP2, DC1). It is opened as this one
                // was, with DeckCraft's schema, resuming where this link stands; `Handover` sets
                // the kept id space on it.
                let url = message.relay_url.clone();
                let hub = deckcraft_ui_egui::live::renewing(hub, move |r: &craft_collab::Reopen<'_>| {
                    craft_collab::web::WebHub::connect_as(&url, r.ticket, r.applied, Some(&deckcraft_live::schema()))
                });
                let base = BaseFile { bytes, name: &message.file_name, expected_sha256: Some(hash) };
                DeckLive::start(hub, &message.room, &base, access, &uid, &mut app.session).map_err(|e| (fail_text(&e), Some(e)))
            });
        match started {
            Ok(mut live) => {
                // The page's clock (Unix ms) for the link and the hold budget, and its local time
                // for the unmerged copy's name.
                live.set_clock(Box::new(js_sys::Date::now));
                live.set_wall_clock(Box::new(local_now));
                app.start_live(live);
                self.joining.borrow_mut().joined(&message.room, access);
                bridge.live_on.set(true);
                if let Some(ticket) = self.pending.take() {
                    refresh_ticket(app, &ticket);
                }
            }
            Err((e, fail)) => {
                // The page's next `live` (its refresh) tries again, unless the file can never be
                // edited live (Review M3).
                self.joining.borrow_mut().start_failed(fail.as_ref());
                self.pending = None;
                log::error!("live editing did not start: {e}");
                not_opened(app, &e);
            }
        }
    }

    /// `live-state` to the page: at once when the state or its reason changes, otherwise at most
    /// once a second (peers, save marker).
    fn report(&mut self, app: &SlideApp, bridge: &Bridge) {
        let Some(ui) = app.live.as_ref() else {
            // The session ended in the window (the file closed): tell the page once (Review M1).
            if bridge.live_on.replace(false) {
                let closed = LiveState {
                    state: HostStatus::Refused,
                    reason: Some(deckcraft_ui_egui::live::tr(deckcraft_live::session::CLOSED_TEXT).to_owned()),
                    saved_at: None,
                    peers: Vec::new(),
                };
                if let Err(e) = bridge.host.post_live_state(&closed) {
                    log::warn!("live-state not delivered: {e}");
                }
                self.reported = None;
            }
            return;
        };
        let now = js_sys::Date::now();
        let status = ui.live().status();
        if let LiveStatus::Live { saved_at: Some(marker) } = status
            && self.saved.as_ref().is_none_or(|(seen, _)| seen != marker)
        {
            self.saved = Some((marker.clone(), now));
        }
        let (state, reason) = crate::live_ticket::host_status(status);
        let report = LiveState {
            state,
            reason,
            saved_at: self.saved.as_ref().and_then(|(_, ms)| js_sys::Date::new(&JsValue::from_f64(*ms)).to_iso_string().as_string()),
            peers: ui.peers().iter().map(|p| HostPeer { id: p.id.clone(), display: p.display.clone() }).collect(),
        };
        let due = match &self.reported {
            None => true,
            Some(last) if last.state == report => false,
            Some(last) => last.state.state != report.state || last.state.reason != report.reason || now - last.at >= REPORT_EVERY_MS,
        };
        if due {
            if let Err(e) = bridge.host.post_live_state(&report) {
                log::warn!("live-state not delivered: {e}");
            }
            self.reported = Some(Reported { state: report, at: now });
        }
    }
}

/// The page's local date and time.
fn local_now() -> chrono::NaiveDateTime {
    let d = js_sys::Date::new_0();
    chrono::NaiveDate::from_ymd_opt(i32::try_from(d.get_full_year()).unwrap_or(1970), d.get_month() + 1, d.get_date())
        .and_then(|day| day.and_hms_opt(d.get_hours(), d.get_minutes(), d.get_seconds()))
        .unwrap_or_default()
}

/// A fresh ticket for the hub; false when the window is not live yet.
fn refresh_ticket(app: &mut SlideApp, ticket: &str) -> bool {
    use craft_collab::Hub as _;
    match app.live.as_mut() {
        Some(live) => {
            live.live_mut().core_mut().hub_mut().set_token(ticket.to_owned());
            true
        }
        None => false,
    }
}

fn tell(app: &mut SlideApp, text: &str) {
    let text = deckcraft_ui_egui::live::tr(text).to_owned();
    app.notify(text.clone());
    app.set_status(text);
}

fn not_opened(app: &mut SlideApp, error: &str) {
    let tr = deckcraft_ui_egui::live::tr;
    let text = format!("{} {}", tr(NOT_OPENED_TEXT).replace("{error}", error), tr(crate::live_ticket::SAVE_OFF_AFTER_FAILED_JOIN));
    app.notify(text.clone());
    app.set_status(text);
}

/// Why a session did not start, for the user.
fn fail_text(fail: &LiveFail) -> String {
    match fail {
        LiveFail::BaseMismatch => deckcraft_ui_egui::live::tr(crate::live_ticket::BASE_CHANGED).to_owned(),
        LiveFail::TooLarge => deckcraft_ui_egui::live::tr(deckcraft_live::session::TOO_LARGE_TEXT).to_owned(),
        other => other.to_string(),
    }
}

/// The relay URL must be on the page's own host (a relative `/craft` is), so the ticket goes
/// nowhere else.
fn relay_on_page_host(relay_url: &str) -> Result<(), String> {
    let window = web_sys::window().ok_or("no window")?;
    let location = window.location();
    let page = location.href().map_err(|_| "no page address")?;
    let page_host = location.host().map_err(|_| "no page host")?;
    let url = web_sys::Url::new_with_base(relay_url, &page).map_err(|_| "the relay address is not a URL")?;
    if crate::live_ticket::same_host(&url.protocol(), &url.host(), &page_host) { Ok(()) } else { Err("the relay is not on this site".into()) }
}

/// Waits `ms` milliseconds on the page's timer.
async fn sleep(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// The base file, fetched with the user's cookies from this page's own origin only. A 404 is
/// asked again a few times (the file may be on its way), nothing else is.
async fn fetch_file(file_url: &str) -> Result<Vec<u8>, String> {
    let window = web_sys::window().ok_or("no window")?;
    let location = window.location();
    let page_origin = location.origin().map_err(|_| "no page origin")?;
    let page = location.href().map_err(|_| "no page address")?;
    let url = web_sys::Url::new_with_base(file_url, &page).map_err(|_| "the file address is not a URL")?;
    if !crate::live_ticket::same_origin(&url.protocol(), &url.origin(), &page_origin) {
        return Err("the file is not on this site".into());
    }
    // One deadline for the whole fetch, retries and body included: aborting settles a hung fetch.
    let abort = web_sys::AbortController::new().map_err(|_| "the file could not be fetched")?;
    let signal = abort.signal();
    let timer = abort.clone();
    let on_timeout = wasm_bindgen::closure::Closure::once_into_js(move || timer.abort());
    let timeout = window.set_timeout_with_callback_and_timeout_and_arguments_0(on_timeout.unchecked_ref(), BASE_TIMEOUT_MS).ok();
    let fetched = fetch_bytes(&window, &url.href(), &signal).await;
    if let Some(id) = timeout {
        window.clear_timeout_with_handle(id);
    }
    match fetched {
        Err(_) if signal.aborted() => Err(deckcraft_ui_egui::live::tr(BASE_TIMEOUT).to_owned()),
        other => other,
    }
}

async fn fetch_bytes(window: &web_sys::Window, href: &str, signal: &web_sys::AbortSignal) -> Result<Vec<u8>, String> {
    let mut tries = 0;
    let response = loop {
        let init = web_sys::RequestInit::new();
        init.set_signal(Some(signal));
        init.set_method("GET");
        init.set_mode(web_sys::RequestMode::SameOrigin);
        init.set_credentials(web_sys::RequestCredentials::SameOrigin);
        let response =
            wasm_bindgen_futures::JsFuture::from(window.fetch_with_str_and_init(href, &init)).await.map_err(|_| "the file could not be fetched")?;
        let response: web_sys::Response = response.dyn_into().map_err(|_| "the fetch gave no response")?;
        if response.status() == 404 && tries < BASE_RETRIES {
            tries += 1;
            sleep(BASE_RETRY_MS).await;
            continue;
        }
        break response;
    };
    if !response.ok() {
        return Err(format!("the server answered {}", response.status()));
    }
    let too_big = || "the file is 2 GiB or larger".to_owned();
    if let Ok(Some(length)) = response.headers().get("content-length")
        && length.trim().parse::<u64>().is_ok_and(|n| n > MAX_BASE_BYTES)
    {
        return Err(too_big());
    }
    let buffer = response.array_buffer().map_err(|_| "the file could not be read")?;
    let buffer = wasm_bindgen_futures::JsFuture::from(buffer).await.map_err(|_| "the file could not be read")?;
    let array = js_sys::Uint8Array::new(&buffer);
    if u64::from(array.length()) > MAX_BASE_BYTES {
        return Err(too_big());
    }
    Ok(array.to_vec())
}
