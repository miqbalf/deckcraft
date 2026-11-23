//! [`DeckLive`]: a DeckCraft presentation in a live room. It owns the `craft-live` link, the
//! mapping between the engine and the room's document and the command gate; the host owns the
//! engine [`Session`] and passes it to each call.

use std::sync::{Arc, Mutex, PoisonError};

use craft_collab::{Hub, LiveError, Replica};
use craft_live::{Access, LiveEditing, LiveStatus, Peer as LivePeer};
use deckcraft_engine::{Edited, Session};
use deckcraft_model::Presentation;
use serde_json::{Value, json};

use crate::app::{DeckApp, Mapping, SentGroup, caret_payload};
use crate::doc::{DeckDoc, DeckSeed, Tomb};
use crate::gate::{Class, MEDIA_TEXT, NOT_YET_TEXT, class};
use crate::kind::{Deck, DeckEdit, DeckPos};
use crate::remote::Targets;
use crate::schema::MAX_SEED_BYTES;
use crate::seed::base_sha256;
use crate::tomb::{Tombs, line_for, tombstoned};
use crate::unmerged::{copy_bytes, local_now};

/// Another editor, with the selection as the host draws it.
pub type Peer = LivePeer<DeckPos>;

/// Why a minting command is refused: this peer has no ids left to give new shapes and slides
/// (its block is used up, or the room's server handed out a space past the block range). A reopen
/// takes a space from the same room, so the text does not offer it (rule 15, L5 review I2).
pub const NO_SPACE_TEXT: &str = "New shapes and slides can't be added in this live session. Save a copy to keep your work; adding more needs a new live session for this file, or help from support.";
/// Why a presentation over the seed limit opens for one person (Ruling D9).
pub const TOO_LARGE_TEXT: &str = "This presentation is too large to edit live. It is open for you alone.";
/// Local edits the other editors never got were kept as a copy: the host saves it
/// ([`DeckLive::take_unmerged`]).
pub const UNMERGED_TEXT: &str = "Some of your changes could not be merged with the other editors. A copy with them is ready to save.";
/// Why a Restore was not shared, when no more specific reason is known.
pub const RESTORE_FAILED_TEXT: &str = "The deleted item could not be brought back. Try again.";
/// Why a shared command is not taken now: the link is not live, or the ticket is read-only.
pub const PAUSED_TEXT: &str = "Editing is paused until the link to the other editors is back";
/// Why the link ended: the live presentation was closed here.
pub const CLOSED_TEXT: &str = "The live presentation was closed";
/// Why nothing can be edited with a read-only ticket.
pub const READ_ONLY_TEXT: &str = "This file is read-only for you.";

/// How long a remote change may wait for a twist whose end never came before it is ended
/// (D4 review M3), in ms; without a clock, in pumps.
const HOLD_BUDGET_MS: f64 = 10_000.0;
const HOLD_BUDGET_PUMPS: f64 = 600.0;

/// The base file every peer of the room opens.
pub struct BaseFile<'a> {
    pub bytes: &'a [u8],
    /// The file's name; only `.pptx` is edited live (Ruling D13).
    pub name: &'a str,
    /// The hash the room says the base has; checked before anything is opened.
    pub expected_sha256: Option<[u8; 32]>,
}

/// Why a session did not start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveFail {
    /// The base did not open as a presentation (or is not a `.pptx`).
    Open(String),
    /// The base bytes are not the file the room was opened on.
    BaseMismatch,
    /// The presentation is over the seed limit: it is open in the session for one person
    /// ([`TOO_LARGE_TEXT`]).
    TooLarge,
    Live(LiveError),
}

impl std::fmt::Display for LiveFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LiveFail::Open(e) => write!(f, "{e}"),
            LiveFail::BaseMismatch => write!(f, "the file is not the one the live session was opened on"),
            LiveFail::TooLarge => write!(f, "{TOO_LARGE_TEXT}"),
            LiveFail::Live(e) => write!(f, "{e}"),
        }
    }
}

/// A line for the person, with a Restore action when it is about something deleted (L5).
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub text: String,
    pub restore: Option<Tomb>,
}

type Clock = Box<dyn FnMut() -> f64 + Send>;
type WallClock = Box<dyn FnMut() -> chrono::NaiveDateTime + Send>;

/// A DeckCraft presentation edited live. Holds `LiveEditing<H, Deck>` directly, so the worker
/// uses it as it is (Ruling D3).
pub struct DeckLive<H: Hub> {
    core: LiveEditing<H, Deck>,
    map: Mapping,
    clock: Arc<Mutex<Option<Clock>>>,
    pumps: f64,
    resync_told: bool,
    /// The live presentation was closed here: the session is over.
    closed: bool,
    /// The person's local date and time, for the unmerged copy's name.
    wall: Option<WallClock>,
}

impl<H: Hub> DeckLive<H> {
    /// Opens `base` as the session's document with the reader `file.open` uses, checks its sha256,
    /// refuses a presentation over [`MAX_SEED_BYTES`] (it stays open for one person), installs the
    /// edit observer and the live undo hook (bound to this document, before any remote change
    /// lands), and seeds the replica. The id block is set when the relay grants a space.
    ///
    /// # Errors
    /// [`LiveFail`].
    pub fn start(hub: H, room: &str, base: &BaseFile, access: Access, uid: &str, session: &mut Session) -> Result<Self, LiveFail> {
        let sha = base_sha256(base.bytes);
        if base.expected_sha256.is_some_and(|want| want != sha) {
            return Err(LiveFail::BaseMismatch);
        }
        if !base.name.to_ascii_lowercase().ends_with(".pptx") {
            return Err(LiveFail::Open("only .pptx files are edited live".into()));
        }
        let deck = deckcraft_engine::cmd::file::open_presentation(base.name, base.bytes).map_err(|e| LiveFail::Open(e.to_string()))?;
        let deck = Arc::new(deck);
        let seed = DeckSeed::new(base.bytes, deck.clone());
        let replica = DeckDoc::seeded(&seed).map_err(LiveFail::Live)?;
        let too_large = replica.seed_bytes() > MAX_SEED_BYTES;
        drop(replica);
        let name = std::path::Path::new(base.name).file_name().map_or_else(|| base.name.to_owned(), |n| n.to_string_lossy().into_owned());
        let i = deckcraft_engine::cmd::file::add_opened(session, &name, None, (*deck).clone());
        if too_large {
            return Err(LiveFail::TooLarge);
        }
        let doc = session.documents().get(i).map(|d| d.uid).ok_or_else(|| LiveFail::Open("the presentation did not open".into()))?;
        if let Some(st) = session.document_mut(doc) {
            // An empty block until the relay grants a space: the engine mints no id from it, so
            // nothing made before the grant can clash with a peer's ids (D2 review Minor 2).
            Arc::make_mut(&mut st.doc).id_block = Some(deckcraft_model::IdBlock::default());
        }
        let core = match LiveEditing::<H, Deck>::start(hub, room, &seed, access) {
            Ok(c) => c,
            Err(e) => {
                session.close_document(i);
                return Err(LiveFail::Live(e));
            }
        };
        let map = Mapping {
            doc,
            base: deck,
            uid: uid.to_owned(),
            space: 0,
            block: None,
            held: Targets::default(),
            held_since: None,
            seen_revision: 0,
            resync_wanted: false,
            touched: Targets::default(),
            partial: false,
            refused: None,
            notices: Vec::new(),
            sent: Vec::new(),
            name,
            tombs: Tombs::default(),
            unmerged: None,
        };
        let mut live =
            DeckLive { core, map, clock: Arc::new(Mutex::new(default_clock())), pumps: 0.0, resync_told: false, closed: false, wall: None };
        if live.clock.lock().unwrap_or_else(PoisonError::into_inner).is_some() {
            live.share_clock();
        }
        live.bind(session);
        Ok(live)
    }

    /// The edit observer and the live undo hook, together, and the engine's own history cleared
    /// (D4 review I-1: its snapshots would bring back a deck without the peers' changes).
    fn bind(&mut self, session: &mut Session) {
        let q = self.core.queue();
        session.set_edit_observer(self.map.doc, Some(Box::new(move |e: Edited| q.push(DeckEdit::Command(Box::new(e))))));
        let q = self.core.queue();
        session.set_live_undo_hook(
            self.map.doc,
            Some(Box::new(move |redo| {
                q.push(if redo { DeckEdit::Redo } else { DeckEdit::Undo });
                true
            })),
        );
        self.clear_history(session);
    }

    fn clear_history(&self, session: &mut Session) {
        if let Some(st) = session.document_mut(self.map.doc) {
            st.history.undo.clear();
            st.history.redo.clear();
            st.history.costs.clear();
        }
    }

    fn share_clock(&mut self) {
        let c = self.clock.clone();
        self.core.set_clock(Box::new(move || c.lock().unwrap_or_else(PoisonError::into_inner).as_mut().map_or(0.0, |f| f())));
    }

    fn now(&mut self) -> f64 {
        self.clock.lock().unwrap_or_else(PoisonError::into_inner).as_mut().map_or(self.pumps, |f| f())
    }

    fn budget(&self) -> f64 {
        if self.clock.lock().unwrap_or_else(PoisonError::into_inner).is_some() { HOLD_BUDGET_MS } else { HOLD_BUDGET_PUMPS }
    }

    fn activate(&self, session: &mut Session) -> Result<(), String> {
        let i =
            session.documents().iter().position(|d| d.uid == self.map.doc).ok_or_else(|| "The live presentation is not open any more".to_owned())?;
        if session.active_index() != Some(i) {
            session.set_active(i);
        }
        Ok(())
    }

    /// One frame: shares the local edits, takes in the other editors' edits and carets, lands
    /// what a drag or a composition held, and says who else is here.
    pub fn pump(&mut self, session: &mut Session) -> Vec<Peer> {
        if self.closed {
            return Vec::new();
        }
        self.pumps += 1.0;
        let now = self.now();
        self.end_stale_twist(session, now);
        let peers = {
            let mut app = DeckApp { session, map: &mut self.map, now };
            self.core.pump(&mut app)
        };
        for p in &peers {
            self.map.tombs.names.insert(craft_live::uid_of(&p.id).to_owned(), p.display.clone());
        }
        self.after(session, now);
        peers
    }

    /// Lands what waited (held changes, the touched slides, a resync), clears the engine's
    /// history and ends a link whose replica fell behind for good.
    fn after(&mut self, session: &mut Session, now: f64) {
        let r = {
            let mut app = DeckApp { session, map: &mut self.map, now };
            app.retry(self.core.live().doc())
        };
        if let Err(e) = r {
            log::warn!("deckcraft-live: a held change did not land: {e}");
            self.keep_unconfirmed();
            self.core.end(craft_live::BROKEN);
        }
        self.clear_history(session);
        if self.core.live().doc().is_some_and(DeckDoc::needs_resync) && !self.resync_told {
            // craft-live cannot replace a replica in place at this pin (L3): end the link, and a
            // reopened file builds a new one.
            self.resync_told = true;
            self.core.end(craft_live::BROKEN);
        }
        self.tell_deletes();
        if let Some(u) = self.map.unmerged.as_mut()
            && u.at.is_none()
        {
            u.at = Some(match self.wall.as_mut() {
                Some(f) => f(),
                None => local_now().unwrap_or_else(|| epoch_ms(now)),
            });
        }
    }

    /// Local edits the room does not have yet, kept as the unmerged copy before this session
    /// ends the link itself: craft-live hands them over only for the breaks it makes in a pump
    /// (L5 review I1).
    ///
    /// Nothing is kept while Live: what was sent is on the wire and will be ordered (re-review
    /// N2); nor once Broken: craft-live hands the edits over at every break that drops them, and
    /// its count stays above 0 for the rest of the session (re-review N1).
    fn keep_unconfirmed(&mut self) {
        if matches!(self.core.status(), LiveStatus::Live { .. } | LiveStatus::Broken(_) | LiveStatus::ReadOnly) {
            return;
        }
        if self.core.live().unconfirmed_edits() == 0 {
            return;
        }
        if let Some(state) = self.replica().map(DeckDoc::state) {
            self.map.keep_unmerged(state);
        }
    }

    /// The deletes found in this pump, as notices with Restore and the deleter's display name.
    fn tell_deletes(&mut self) {
        let pending = std::mem::take(&mut self.map.tombs.pending);
        for (t, by) in pending {
            let text = line_for(t, &self.map.tombs.name(&by));
            self.map.notices.push(Notice { text, restore: Some(t) });
        }
    }

    /// A remote change held past its budget by a twist whose `gesture: end` never came: end the
    /// twist (D4 review M3). The budget counts from the hold or from the gesture's last step, so
    /// a twist still turning is never cut; a drag or a composition the person is in is left
    /// alone. The person's active document stays the active one.
    fn end_stale_twist(&mut self, session: &mut Session, now: f64) {
        let Some(st) = session.documents().iter().find(|d| d.uid == self.map.doc) else { return };
        let (revision, twist) = (st.revision, st.interaction.as_ref().is_some_and(|i| i.label == deckcraft_engine::tools::TWIST));
        let moved = std::mem::replace(&mut self.map.seen_revision, revision) != revision;
        let Some(since) = self.map.held_since else { return };
        if moved {
            self.map.held_since = Some(now);
            return;
        }
        if now - since < self.budget() || session.tool.dragging() || !twist {
            return;
        }
        let was = session.active_index();
        if self.activate(session).is_err() {
            return;
        }
        if let Err(e) = session.execute("shape.rotate", &json!({"gesture": "end"})) {
            log::warn!("deckcraft-live: ending a stale twist failed: {e}");
        }
        if let Some(i) = was
            && session.active_index() != Some(i)
        {
            session.set_active(i);
        }
        self.map.held_since = Some(now);
    }

    fn flush(&mut self, session: &mut Session) {
        let now = self.now();
        {
            let mut app = DeckApp { session, map: &mut self.map, now };
            self.core.flush(&mut app);
        }
        self.after(session, now);
    }

    /// The gate. LOCAL and FREE commands run now, then flush, so each command is shared alone;
    /// NOT_YET and MEDIA commands are refused and the presentation is untouched. A shared
    /// command the safety net refuses is put back, and its reason is the answer.
    ///
    /// # Errors
    /// The refusal or the command's own error, as text for the person.
    pub fn run(&mut self, session: &mut Session, id: &str, params: &Value) -> Result<Value, String> {
        if self.closed {
            return Err(CLOSED_TEXT.to_owned());
        }
        match class(id) {
            Class::NotYet => return Err(NOT_YET_TEXT.to_owned()),
            Class::Media => return Err(MEDIA_TEXT.to_owned()),
            Class::Free | Class::History if !self.core.editable() => return Err(self.core.not_editable()),
            Class::Free | Class::History | Class::Local => {}
        }
        self.activate(session)?;
        if id == "file.close" {
            // Closing the live presentation ends the session first, as `stop` does. Edits still
            // queued go out first; those not yet in the room (made while reconnecting) are kept
            // as the unmerged copy.
            self.flush(session);
            self.keep_unconfirmed();
            self.detach(session);
            self.core.end(CLOSED_TEXT);
            self.closed = true;
            return session.execute(id, params).map_err(|e| e.to_string());
        }
        self.map.refused = None;
        let done = session.execute(id, params);
        self.flush(session);
        if let Some(why) = self.map.refused.take() {
            return Err(why);
        }
        done.map_err(|e| {
            let e = e.to_string();
            match (e.contains(deckcraft_engine::cmd::NO_ID_SPACE), self.map.block.is_some()) {
                (true, true) => NO_SPACE_TEXT.to_owned(),
                // A space was granted outside the block range: no block will come (N2).
                (true, false) if self.map.space != 0 => NO_SPACE_TEXT.to_owned(),
                // No space granted yet: the empty block minted nothing.
                (true, false) => craft_live::CONNECTING.to_owned(),
                (false, _) => e,
            }
        })
    }

    /// Whether the host may offer `id` now; `Err(reason)` greys it out.
    ///
    /// # Errors
    /// Why `id` is not available.
    pub fn allows(&self, id: &str) -> Result<(), &'static str> {
        match class(id) {
            Class::Local => Ok(()),
            Class::NotYet => Err(NOT_YET_TEXT),
            Class::Media => Err(MEDIA_TEXT),
            Class::Free | Class::History if self.core.editable() => Ok(()),
            Class::Free | Class::History if matches!(self.core.status(), LiveStatus::ReadOnly) => Err(READ_ONLY_TEXT),
            // Live or reconnecting, but too much waits to be sent (C9).
            Class::Free | Class::History if self.core.not_editable() == craft_live::BUFFER_FULL => Err(craft_live::BUFFER_FULL),
            Class::Free | Class::History => Err(PAUSED_TEXT),
        }
    }

    #[must_use]
    pub fn status(&self) -> &LiveStatus {
        self.core.status()
    }

    /// Lines for the person since the last call: the link's own and the mapping's.
    pub fn take_notices(&mut self) -> Vec<Notice> {
        let mut out: Vec<Notice> = self.core.take_notices().into_iter().map(|text| Notice { text, restore: None }).collect();
        out.append(&mut self.map.notices);
        out
    }

    #[must_use]
    pub fn editable(&self) -> bool {
        self.core.editable()
    }

    /// Tells the room where this window's selection and caret are (held to five a second).
    pub fn send_selection(&mut self, session: &Session) {
        let Some(st) = session.documents().iter().find(|d| d.uid == self.map.doc) else { return };
        if let Some(p) = caret_payload(st) {
            self.core.send_presence(p);
        }
    }

    /// Brings back a deleted slide or shape: `del = false` for it, for the groups above a shape
    /// and for its slide when they are deleted too, as one new group. Every peer shows it again,
    /// with what was typed into it meanwhile. Nothing to do when it is not deleted.
    ///
    /// # Errors
    /// Why it cannot be done now (the link, read-only, closed).
    pub fn restore(&mut self, session: &mut Session, target: Tomb) -> Result<(), String> {
        if self.closed {
            return Err(CLOSED_TEXT.to_owned());
        }
        if !self.core.editable() {
            return Err(self.core.not_editable());
        }
        if self.replica().is_none_or(|r| tombstoned(r, target).is_none()) {
            return Ok(());
        }
        self.map.refused = None;
        self.core.queue().push(DeckEdit::Restore(target));
        self.flush(session);
        if let Some(why) = self.map.refused.take() {
            return Err(why);
        }
        // craft-live can still refuse the group (the buffer filled, the link dropped): then the
        // target is deleted still (L5 review M7).
        if self.replica().is_some_and(|r| tombstoned(r, target).is_some()) {
            let live = self.core.live();
            return Err(if !self.core.editable() {
                self.core.not_editable()
            } else if live.buffer_full() {
                craft_live::BUFFER_FULL.to_owned()
            } else if !live.ready_to_type() {
                // The replica is damaged or given up (L5 re-review N4).
                craft_live::BROKEN.to_owned()
            } else {
                RESTORE_FAILED_TEXT.to_owned()
            });
        }
        Ok(())
    }

    /// The `.pptx` bytes of the base + this replica's state, with the local edits craft-live was
    /// about to drop (a reset, a replica given up), and the copy's name ([`unmerged_name`]);
    /// None otherwise. Taken once; every hand-over since the last take is in it. A notice
    /// ([`UNMERGED_TEXT`]) says when one starts waiting: hosts call this after every pump.
    ///
    /// [`unmerged_name`]: crate::unmerged_name
    pub fn take_unmerged(&mut self) -> Option<(String, Vec<u8>)> {
        let u = self.map.unmerged.take()?;
        let at = u.at.or_else(local_now).unwrap_or_default();
        match copy_bytes(&self.map.base, &u.states) {
            Ok(bytes) => Some((crate::unmerged::unmerged_name(&self.map.name, at), bytes)),
            Err(e) => {
                log::warn!("deckcraft-live: the unmerged copy could not be built: {e}");
                None
            }
        }
    }

    /// The person's local date and time, for the unmerged copy's name (a browser host passes
    /// the page's; native hosts have the system's by default).
    pub fn set_wall_clock(&mut self, clock: Box<dyn FnMut() -> chrono::NaiveDateTime + Send>) {
        self.wall = Some(clock);
    }

    /// The clock the link and the hold budget run on, in ms (a browser host passes the page's).
    pub fn set_clock(&mut self, clock: Box<dyn FnMut() -> f64 + Send>) {
        *self.clock.lock().unwrap_or_else(PoisonError::into_inner) = Some(clock);
        self.share_clock();
    }

    #[must_use]
    pub fn core(&self) -> &LiveEditing<H, Deck> {
        &self.core
    }

    pub fn core_mut(&mut self) -> &mut LiveEditing<H, Deck> {
        &mut self.core
    }

    /// The replica, for tests and the worker.
    #[must_use]
    pub fn replica(&self) -> Option<&DeckDoc> {
        self.core.live().doc()
    }

    /// The base presentation every peer opened.
    #[must_use]
    pub fn base(&self) -> &Arc<Presentation> {
        &self.map.base
    }

    /// The engine document's uid.
    #[must_use]
    pub fn doc(&self) -> u64 {
        self.map.doc
    }

    /// The commands sent since the last call, one group each however many updates it took.
    pub fn take_sent(&mut self) -> Vec<SentGroup> {
        std::mem::take(&mut self.map.sent)
    }

    /// Remote changes are waiting for a drag or a composition to end.
    #[must_use]
    pub fn holding(&self) -> bool {
        !self.map.held.is_empty() || self.map.resync_wanted
    }

    /// Ends the session: the observer and the undo hook come off, the engine's history is cleared
    /// (its snapshots predate the peers' changes) and the id block is dropped, so the
    /// presentation is an ordinary one again. Returns the unmerged copy ([`DeckLive::take_unmerged`])
    /// when local edits never reached the room: the host saves it, as for any other copy.
    #[must_use = "the unmerged copy holds edits the other editors never got"]
    pub fn stop(mut self, session: &mut Session) -> Option<(String, Vec<u8>)> {
        if !self.closed {
            // As `file.close` does: queued edits go out, those the room lacks are kept.
            self.flush(session);
            self.keep_unconfirmed();
        }
        self.detach(session);
        self.take_unmerged()
    }

    fn detach(&mut self, session: &mut Session) {
        session.set_edit_observer(self.map.doc, None);
        session.set_live_undo_hook(self.map.doc, None);
        self.clear_history(session);
        if let Some(st) = session.document_mut(self.map.doc)
            && st.doc.id_block.is_some()
        {
            Arc::make_mut(&mut st.doc).id_block = None;
        }
    }
}

/// `ms` on the session's clock read as Unix time (a web host's page clock), when nothing better
/// is known.
fn epoch_ms(ms: f64) -> chrono::NaiveDateTime {
    let ms = ms as i64;
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.naive_utc()).unwrap_or_default()
}

#[cfg(not(target_arch = "wasm32"))]
fn default_clock() -> Option<Clock> {
    let start = std::time::Instant::now();
    Some(Box::new(move || start.elapsed().as_secs_f64() * 1000.0))
}

/// `std::time` has no clock on the web; the host gives one with [`DeckLive::set_clock`].
#[cfg(target_arch = "wasm32")]
fn default_clock() -> Option<Clock> {
    None
}
