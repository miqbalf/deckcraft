//! Live co-editing in the window: the presentation a host opened with
//! [`deckcraft_live::DeckLive`], pumped every frame, with its status, notices and the other
//! editors shown.
//!
//! - Commands: while the live presentation is the active one, [`SlideApp::run`] sends engine
//!   commands through [`DeckLive::run`] (the gate, then one shared group per command), and the
//!   ribbon, menus and Quick Access Toolbar grey what [`LiveUi::gate`] refuses, with the reason as
//!   the tooltip.
//! - The live strip above the status bar shows the link's status, the newest notices (a notice
//!   about something deleted gets a [`RESTORE_LABEL`] button) and the other editors' badges; a
//!   badge click goes to that editor's slide and selects what they selected.
//! - On the canvas, each editor's selection is outlined in their colour with a name tag, and their
//!   caret is a bar in the same colour; the thumbnail pane and the sorter put a dot on each slide
//!   an editor is on. Each is an accessibility node with the person's name.
//!
//! User-facing lines are English keys; [`tr`] is where DeckCraft's translation lookup goes once it
//! has one. The keys this module adds are [`ENGLISH_KEYS`].

use std::collections::HashMap;

use craft_collab::{ClientMsg, Hub, ServerMsg};
use craft_live::LiveStatus;
use craft_ui::presence::{Peer as Badge, PresenceBar, colours_for};
use deckcraft_engine::{Session, TextSel};
use deckcraft_geom::Point;
use deckcraft_live::{DeckLive, Notice, Peer, SECTIONS_TEXT, TextTarget, Tomb};
use deckcraft_model::ShapeId;
use egui::{Color32, CornerRadius, Pos2, Rect, Sense, Stroke, Ui, pos2, vec2};
use serde_json::{Value, json};

use crate::SlideApp;
use crate::canvas::Xf;
use crate::theme::{self, Tokens};

/// A boxed hub, so a host hands in whichever transport it has (socket, browser, test relay).
pub struct BoxHub(pub Box<dyn Hub>);

impl Hub for BoxHub {
    fn send(&mut self, msg: ClientMsg) -> craft_collab::Result<()> {
        self.0.send(msg)
    }
    fn poll(&mut self) -> Vec<ServerMsg> {
        self.0.poll()
    }
    fn connected(&self) -> bool {
        self.0.connected()
    }
    fn set_token(&mut self, token: String) {
        self.0.set_token(token);
    }
    fn set_space(&mut self, space: u32) {
        self.0.set_space(space);
    }
}

/// The live presentation as the window holds it.
pub type LiveDeck = DeckLive<BoxHub>;

pub use deckcraft_live::RESTORE_LABEL;
/// The live strip's status for a person with a read-only ticket.
pub const VIEWING_TEXT: &str = "Viewing";
/// The live strip's status while connected and editing.
pub const LIVE_TEXT: &str = "Editing live";
/// Saves the copy of edits the room could not merge ([`deckcraft_live::UNMERGED_TEXT`]).
pub const SAVE_COPY_TEXT: &str = "Save a copy";
/// Closes a notice.
pub const DISMISS_TEXT: &str = "Dismiss";
/// The accessible name of one editor's selected shape: `{name}`, `{shape}`, `{slide}`.
pub const SELECTION_NAME: &str = "{name}'s selection, {shape} on slide {slide}";
/// The accessible name of one editor's text caret: `{name}`, `{shape}`.
pub const CURSOR_NAME: &str = "{name}'s cursor in {shape}";
/// The accessible name of the dot on a slide an editor is on: `{name}`, `{slide}`.
pub const ON_SLIDE_NAME: &str = "{name} on slide {slide}";

/// Why nothing is taken once the link to the room ended for a reason the window has no line for.
pub const LINK_ENDED_TEXT: &str = "The link to the other editors ended. Reopen the file to keep editing.";

/// Said when the window left the room (a close that was then held back for a waiting copy).
pub const LIVE_ENDED_TEXT: &str = "Live editing has ended in this window.";

/// Said when a badge is clicked for an editor whose place this window cannot show: `{name}`.
pub const NO_PLACE_TEXT: &str = "{name}'s place can't be shown yet. It shows again when they select something.";

/// Said when the page stored a copy of unmerged edits: `{name}`.
pub const COPY_SAVED_TEXT: &str = "Your unmerged changes were saved as {name}.";
/// Said when the page could not store a copy of unmerged edits: `{error}`.
pub const COPY_FAILED_TEXT: &str = "Your unmerged changes could not be saved to Nextcloud: {error}. Use Save a copy to download them.";

/// Every English key this module draws, for the translation report.
pub const ENGLISH_KEYS: &[&str] = &[
    RESTORE_LABEL,
    VIEWING_TEXT,
    LIVE_TEXT,
    SAVE_COPY_TEXT,
    DISMISS_TEXT,
    SELECTION_NAME,
    CURSOR_NAME,
    ON_SLIDE_NAME,
    LINK_ENDED_TEXT,
    LIVE_ENDED_TEXT,
    NO_PLACE_TEXT,
    COPY_SAVED_TEXT,
    COPY_FAILED_TEXT,
];

/// The translation lookup for an English key. DeckCraft has no translation catalogue yet, so this
/// is English as it stands; every live line goes through it so a catalogue needs one change.
#[must_use]
pub fn tr(key: &str) -> &str {
    key
}

/// Whether a control is offered, and, when the live gate greys it, why: passed with the control
/// so the reason reaches exactly that control (no per-thread slot).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Gate {
    pub on: bool,
    /// The live reason, an English key, when the live gate refused the command.
    pub why: Option<&'static str>,
}

impl From<bool> for Gate {
    fn from(on: bool) -> Gate {
        Gate { on, why: None }
    }
}

/// Gives a greyed control the live reason as its tooltip, when the live gate greyed it.
pub(crate) fn gate_hint(resp: egui::Response, gate: &Gate) -> egui::Response {
    match gate.why {
        Some(why) if !gate.on => resp.on_hover_text(tr(why)),
        _ => resp,
    }
}

/// A menu entry for engine command `id`, greyed when it cannot run, with the live reason as its
/// tooltip when the live gate greyed it.
pub(crate) fn menu_button(app: &SlideApp, ui: &mut Ui, label: &str, id: &str) -> egui::Response {
    let gate = crate::ribbon::gate(app, id);
    let r = ui.add_enabled(gate.on, egui::Button::new(label));
    match gate.why {
        Some(why) => r.on_disabled_hover_text(tr(why)),
        None => r,
    }
}

/// How long a name tag stays after the selection under it last moved, in seconds.
const TAG_SECS: f64 = 3.0;
/// How many notices the strip keeps (the newest is shown).
const NOTICES_KEPT: usize = 3;
/// Commands that add, remove or move slides: in a presentation with sections or custom shows the
/// room cannot share them yet (L4 review Important 3), so they are greyed with [`SECTIONS_TEXT`].
const SLIDE_MOVES: &[&str] = &["slide.delete", "slide.duplicate", "slide.move", "slide.new"];
/// Whether `id` adds or removes slides in the current state, though it is not a slide command:
/// Cut and Delete with slides selected in the thumbnail pane or the sorter (`edit.delete`'s slide
/// scope, which the Delete key runs there), and Paste or Duplicate while slides are on the
/// clipboard and no text is being edited (U1 review M7).
fn moves_slides(session: &Session, id: &str) -> bool {
    let Some(st) = session.active() else { return false };
    let sel = &st.selection;
    let slides_selected = sel.shapes.is_empty() && sel.text.is_none() && !sel.slides.is_empty();
    match id {
        "edit.cut" | "edit.delete" => slides_selected,
        "edit.paste" | "edit.pasteSpecial" => sel.text.is_none() && !session.clipboard.slides.is_empty(),
        _ => false,
    }
}

/// Window commands that end in an engine command: they are offered as that command is.
const UI_GATE: &[(&str, &str)] = &[
    ("app.browseThemeDialog", "design.browseTheme"),
    ("app.changePictureDialog", "picture.change"),
    ("app.compareDialog", "review.compare"),
    ("app.insertAudioDialog", "insert.audio"),
    ("app.insertPictureDialog", "insert.picture"),
    ("app.insertVideoDialog", "insert.video"),
    ("app.pasteSpecial", "edit.pasteSpecial"),
    ("app.reuseSlidesDialog", "slide.reuse"),
    ("app.saveAsDialog", "file.saveAs"),
    ("app.smartArtPictureDialog", "smartArt.picture"),
];

/// What this window last told the room about its selection.
type SelKey = (usize, Vec<ShapeId>, Option<(ShapeId, (usize, usize), (usize, usize), bool)>);

/// The live session in the window, with what it shows of the other editors.
pub struct LiveUi {
    live: LiveDeck,
    peers: Vec<Peer>,
    /// Each editor's colour, the same as the badge's ([`colours_for`] over the room in bar order).
    colours: HashMap<String, Color32>,
    bar: PresenceBar,
    notices: Vec<Notice>,
    sent: Option<SelKey>,
    /// Per editor: a signature of their selection's place and when it last changed (`ui` time).
    moved: HashMap<String, (u64, f64)>,
}

impl LiveUi {
    #[must_use]
    pub fn new(live: LiveDeck) -> LiveUi {
        LiveUi { live, peers: Vec::new(), colours: HashMap::new(), bar: PresenceBar::new(), notices: Vec::new(), sent: None, moved: HashMap::new() }
    }

    #[must_use]
    pub fn live(&self) -> &LiveDeck {
        &self.live
    }

    pub fn live_mut(&mut self) -> &mut LiveDeck {
        &mut self.live
    }

    /// The other editors as of the last pump.
    #[must_use]
    pub fn peers(&self) -> &[Peer] {
        &self.peers
    }

    /// Whether the live presentation is the one the window shows.
    #[must_use]
    pub fn applies(&self, session: &Session) -> bool {
        session.active().is_some_and(|d| d.uid == self.live.doc())
    }

    /// Whether the live presentation is still open in the session.
    fn open(&self, session: &Session) -> bool {
        session.documents().iter().any(|d| d.uid == self.live.doc())
    }

    /// A line for the person (a mapping's notice; also how a test stands in for one).
    pub fn push_notice(&mut self, n: Notice) {
        self.notices.push(n);
        let extra = self.notices.len().saturating_sub(NOTICES_KEPT);
        self.notices.drain(..extra);
    }

    fn take_notices(&mut self) {
        for n in self.live.take_notices() {
            self.push_notice(n);
        }
    }

    /// One frame: share and take in edits, then tell the room where the selection is when it moved.
    /// Returns a copy of local edits the room could not merge, when one was handed over.
    #[must_use]
    pub fn pump(&mut self, session: &mut Session) -> Option<(String, Vec<u8>)> {
        self.peers = self.live.pump(session);
        let ids: Vec<String> = self.peers.iter().map(|p| p.id.clone()).collect();
        self.colours = ids.iter().cloned().zip(colours_for(&ids)).collect();
        self.bar.set(self.peers.iter().map(|p| Badge::new(p.id.clone(), p.display.clone())).collect());
        self.moved.retain(|id, _| ids.contains(id));
        self.take_notices();
        // Taken after every pump: each hand-over since the last take is in the newest copy.
        let copy = self.live.take_unmerged();
        let key = session.documents().iter().find(|d| d.uid == self.live.doc()).map(|st| {
            let sel = &st.selection;
            (sel.slide, sel.shapes.clone(), sel.text.as_ref().map(|t| (t.shape, t.anchor, t.caret, t.notes)))
        });
        if key.is_some() && key != self.sent {
            self.live.send_selection(session);
            self.sent = key;
        }
        copy
    }

    /// Runs an engine command through the gate; the notices it gave are kept.
    ///
    /// # Errors
    /// The refusal, or the command's own error, for the person.
    pub fn run(&mut self, session: &mut Session, id: &str, params: &Value) -> Result<Value, String> {
        let r = match self.gate(session, id) {
            Err(why) => Err(tr(why).to_owned()),
            Ok(()) => self.live.run(session, id, params).map_err(|e| tr(&e).to_owned()),
        };
        self.take_notices();
        r
    }

    /// Whether `id` (an engine command, or a window command that ends in one) may run now.
    ///
    /// # Errors
    /// Why not, as an English key.
    pub fn gate(&self, session: &Session, id: &str) -> Result<(), &'static str> {
        let engine = UI_GATE.iter().find(|(ui, _)| *ui == id).map_or(id, |(_, cmd)| *cmd);
        if deckcraft_engine::find_command(engine).is_none() {
            // The window's own commands (views, panes, zoom) change nothing that is shared.
            return Ok(());
        }
        let sections = session
            .documents()
            .iter()
            .find(|d| d.uid == self.live.doc())
            .is_some_and(|d| !d.doc.sections.is_empty() || !d.doc.custom_shows.is_empty());
        if sections && (SLIDE_MOVES.contains(&engine) || moves_slides(session, engine)) {
            return Err(SECTIONS_TEXT);
        }
        self.live.allows(engine).map_err(|why| self.why_not(why))
    }

    /// The live session says "paused" for every state that takes no edits; a link that is still
    /// connecting, or broke, or was refused, says that instead.
    fn why_not(&self, why: &'static str) -> &'static str {
        if why != deckcraft_live::PAUSED_TEXT {
            return why;
        }
        match self.live.status() {
            LiveStatus::Connecting => craft_live::CONNECTING,
            LiveStatus::Broken(m) | LiveStatus::Refused(m) if m == craft_live::BROKEN => craft_live::BROKEN,
            LiveStatus::Broken(_) | LiveStatus::Refused(_) => LINK_ENDED_TEXT,
            _ => why,
        }
    }

    /// Why typing is not taken now, when it is not.
    #[must_use]
    pub fn typing_refused(&self) -> Option<&'static str> {
        self.live.allows("text.insert").err().map(|why| self.why_not(why))
    }

    /// The status line and, when there is more to say, its tooltip.
    #[must_use]
    pub fn status_line(&self) -> (String, Option<String>) {
        match self.live.status() {
            LiveStatus::Connecting => (tr(craft_live::CONNECTING).to_owned(), None),
            LiveStatus::Live { .. } => (tr(LIVE_TEXT).to_owned(), None),
            LiveStatus::ReadOnly => (tr(VIEWING_TEXT).to_owned(), Some(tr(deckcraft_live::READ_ONLY_TEXT).to_owned())),
            LiveStatus::Reconnecting => (tr(deckcraft_live::PAUSED_TEXT).to_owned(), None),
            LiveStatus::Refused(why) | LiveStatus::Broken(why) => (tr(why).to_owned(), None),
        }
    }

    /// Brings back what a notice was about.
    ///
    /// # Errors
    /// Why it could not be brought back.
    pub fn restore(&mut self, session: &mut Session, target: Tomb) -> Result<(), String> {
        let r = self.live.restore(session, target).map_err(|e| tr(&e).to_owned());
        self.take_notices();
        r
    }

    fn colour(&self, peer: &Peer) -> Color32 {
        self.colours.get(&peer.id).copied().unwrap_or_else(|| craft_ui::presence::colour_for(&peer.id))
    }
}

impl SlideApp {
    /// Hands the window a presentation the host opened live on this window's session. It is
    /// shown, and it takes the place of the empty presentation host mode started with
    /// ([`Self::start_for_host`]) when that one is still open, untitled and unchanged.
    pub fn start_live(&mut self, live: LiveDeck) {
        let doc = live.doc();
        self.live = Some(LiveUi::new(live));
        let blank = self.host_start.take().filter(|&uid| uid != doc);
        if let Some(i) = blank.and_then(|uid| self.session.documents().iter().position(|d| d.uid == uid && d.path.is_none() && !d.is_dirty())) {
            self.session.close_document(i);
        }
        if let Some(i) = self.session.documents().iter().position(|d| d.uid == doc) {
            self.session.set_active(i);
        }
    }

    /// Ends the live session: what the room never got is kept in [`SlideApp::pending_unmerged`]
    /// (and said), and the window leaves the room. Every way a live presentation ends comes here.
    pub fn end_live(&mut self) {
        let Some(lu) = self.live.take() else { return };
        if let Some(copy) = lu.live.stop(&mut self.session) {
            self.pending_unmerged.push(copy);
        }
        if !self.pending_unmerged.is_empty() {
            self.notify(tr(deckcraft_live::UNMERGED_TEXT));
        }
    }

    /// The window is closing for good: ends the live session and offers each copy still waiting
    /// once. The desktop shell calls it in `on_exit`.
    pub fn finish_live(&mut self) {
        self.end_live();
        self.offer_waiting_copies();
    }

    /// Offers each copy of unmerged edits still waiting once; those the host did not take keep
    /// waiting (the strip offers them). A web host that keeps the files calls it every frame, so
    /// a copy goes to the page as soon as it is made (Ruling D18).
    pub fn offer_waiting_copies(&mut self) {
        let waiting = std::mem::take(&mut self.pending_unmerged);
        for (name, bytes) in waiting {
            if !self.save_unmerged(&name, &bytes) {
                self.pending_unmerged.push((name, bytes));
            }
        }
    }

    /// Whether a copy of unmerged edits is waiting to be saved.
    #[must_use]
    pub fn copies_waiting(&self) -> bool {
        !self.pending_unmerged.is_empty()
    }

    /// Whether some unmerged edits are not stored anywhere yet: a copy waits, or the page has not
    /// confirmed one it was handed.
    #[must_use]
    pub fn copies_unsaved(&self) -> bool {
        !self.pending_unmerged.is_empty() || !self.copies_sent.is_empty()
    }

    /// A web host that keeps the files calls this every frame: each waiting copy goes to the page
    /// as soon as it is made (Ruling D18), until the page could not take one; from then on the
    /// strip's Save a copy downloads them.
    pub fn hand_over_copies(&mut self) {
        if !self.page_refused_copy && !self.pending_unmerged.is_empty() {
            self.offer_waiting_copies();
        }
    }

    /// The page answered a copy it was handed: stored (say under which name), or not (it waits
    /// again, to be downloaded).
    pub(crate) fn copy_answered(&mut self, copy: (String, Vec<u8>), answer: crate::HostAnswer) {
        match answer {
            crate::HostAnswer::Saved { name, .. } => {
                let name = name.unwrap_or_else(|| copy.0.clone());
                self.notify(fill(COPY_SAVED_TEXT, &[("name", &name)]));
            }
            crate::HostAnswer::Failed { error, .. } => {
                self.page_refused_copy = true;
                self.pending_unmerged.push(copy);
                let text = fill(COPY_FAILED_TEXT, &[("error", &error)]);
                self.set_status(text.clone());
                self.notify(text);
            }
        }
    }

    /// The host ended the live session for `reason` (the page reopened the file in another room,
    /// or the ticket lost edit access). Edits the room never confirmed come back as the copy at
    /// the next frame ([`DeckLive::end`]).
    pub fn end_live_for(&mut self, reason: &str) {
        if let Some(lu) = self.live.as_mut() {
            lu.live.end(&mut self.session, reason);
        }
    }

    /// Whether the live session has edits the room has not confirmed (sent while reconnecting, or
    /// within one round trip).
    #[must_use]
    pub fn live_unconfirmed(&self) -> bool {
        // A link that ended handed its unconfirmed edits over as the copy, which counts on its own
        // (U2 fix-1 review N1).
        self.live.as_ref().is_some_and(|lu| {
            !matches!(lu.live.status(), LiveStatus::Broken(_) | LiveStatus::Refused(_)) && lu.live.core().live().unconfirmed_edits() > 0
        })
    }

    /// The window is about to close: the live session ends, and waiting copies are offered.
    /// When one was not taken, the close is held back once so the strip can keep offering it,
    /// and the person is told that live editing has ended.
    pub(crate) fn close_live_for_quit(&mut self, ctx: &egui::Context) {
        let was_live = self.live.is_some();
        self.end_live();
        if self.pending_unmerged.is_empty() || self.unmerged_close_offered {
            return;
        }
        self.unmerged_close_offered = true;
        self.finish_live();
        if !self.pending_unmerged.is_empty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            let mut line = String::new();
            if was_live {
                line.push_str(tr(LIVE_ENDED_TEXT));
                line.push(' ');
            }
            line.push_str(tr(deckcraft_live::UNMERGED_TEXT));
            self.notify(line.clone());
            self.set_status(line);
        }
    }

    /// Takes a live session the host left in [`crate::Services`], ends one whose presentation was
    /// closed (so this window leaves the room at once), and pumps the rest.
    pub(crate) fn pump_live(&mut self, ctx: &egui::Context) {
        if let Some(l) = self.services.live.take() {
            self.start_live(l);
        }
        let Some(lu) = self.live.as_mut() else { return };
        if !lu.open(&self.session) {
            // Closed by a path that did not end the session: end it now and offer the copy.
            self.end_live();
            self.offer_unmerged();
            return;
        }
        if let Some(copy) = lu.pump(&mut self.session) {
            self.pending_unmerged.push(copy);
        }
        // Edits and carets from the room arrive without input: keep frames coming.
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }

    /// Shares what the last input changed now, so it is its own group in the room (as a command
    /// run through [`SlideApp::run_live`] is).
    pub(crate) fn flush_live(&mut self) {
        let Some(lu) = self.live.as_mut().filter(|l| l.applies(&self.session)) else { return };
        if let Some(copy) = lu.pump(&mut self.session) {
            self.pending_unmerged.push(copy);
        }
    }

    /// Offers the oldest waiting copy; it is dropped from the list only once the host took it.
    pub(crate) fn offer_unmerged(&mut self) {
        let Some((name, bytes)) = self.pending_unmerged.first().cloned() else { return };
        if self.save_unmerged(&name, &bytes) {
            self.pending_unmerged.remove(0);
        }
    }

    /// Hands a copy of unmerged edits to the person: a download on the web, a Save dialog on the
    /// desktop. True when the host took it (downloaded, or written).
    pub(crate) fn save_unmerged(&mut self, name: &str, bytes: &[u8]) -> bool {
        if !self.page_refused_copy
            && let Some(save_copy) = self.services.save_copy.as_mut()
        {
            return match save_copy(name, bytes) {
                Ok(Some(id)) => {
                    // Kept until the page says it stored it.
                    self.copies_sent.push((id, (name.to_owned(), bytes.to_vec())));
                    true
                }
                Ok(None) => {
                    self.notify(fill(COPY_SAVED_TEXT, &[("name", name)]));
                    true
                }
                Err(e) => {
                    // Not handed over: no more automatic hand-overs; the strip offers it.
                    self.page_refused_copy = true;
                    self.set_status(e);
                    false
                }
            };
        }
        if self.page_refused_copy
            && let Some(download) = self.services.download_copy.as_mut()
        {
            download(name, bytes);
            return true;
        }
        if let Some(download) = self.services.download.as_mut() {
            download(name, bytes);
            return true;
        }
        let Some(path) = self.services.pick_save.as_mut().and_then(|pick| pick(name)) else {
            self.set_status(tr(deckcraft_live::UNMERGED_TEXT));
            return false;
        };
        let written = match self.services.write.as_mut() {
            Some(write) => write(&path, bytes),
            None => write_file(&path, bytes),
        };
        match written {
            Ok(()) => {
                self.set_status(path);
                true
            }
            Err(e) => {
                self.set_status(e);
                false
            }
        }
    }

    /// Why `id` is greyed now, when the live gate refuses it.
    pub(crate) fn live_refusal(&self, id: &str) -> Option<&'static str> {
        let lu = self.live.as_ref().filter(|l| l.applies(&self.session))?;
        lu.gate(&self.session, id).err()
    }

    /// While the live presentation is active, whether Undo (`edit.undo`) or Redo (`edit.redo`)
    /// has a command of mine to take back or bring back ([`DeckLive::can_undo`]); None for any
    /// other id, or when not live.
    pub(crate) fn live_history(&self, id: &str) -> Option<bool> {
        let lu = self.live.as_ref().filter(|l| l.applies(&self.session))?;
        match id {
            "edit.undo" => Some(lu.live.can_undo()),
            "edit.redo" => Some(lu.live.can_redo()),
            _ => None,
        }
    }

    /// Why typing into the live presentation is not taken now, when it is not.
    pub(crate) fn live_typing_refused(&self) -> Option<&'static str> {
        self.live.as_ref().filter(|l| l.applies(&self.session))?.typing_refused()
    }

    /// Runs an engine command through the live session when the live presentation is active.
    pub(crate) fn run_live(&mut self, id: &str, params: &Value) -> Option<Result<Value, String>> {
        let lu = self.live.as_mut().filter(|l| l.applies(&self.session))?;
        Some(lu.run(&mut self.session, id, params))
    }

    /// Runs an engine command for a panel or dialog: through the live session when the live
    /// presentation is active (one shared group, refused with the reason), else on the session.
    pub(crate) fn run_engine(&mut self, id: &str, params: &Value) -> Result<Value, String> {
        match self.run_live(id, params) {
            Some(r) => r,
            None => self.session.execute(id, params).map_err(|e| e.to_string()),
        }
    }

    /// A drag on the live presentation whose release never came (the window lost focus, the
    /// pointer was cancelled) ends where the pointer last was: a drag left open would hold the
    /// other editors' changes back (L4 concern 2, D4 review M3). A frame that also holds the
    /// release is left to the canvas, which ends the drag at the release.
    pub(crate) fn end_lost_drag(&mut self, ctx: &egui::Context) {
        if !self.session.tool.dragging() || !self.live.as_ref().is_some_and(|l| l.applies(&self.session)) {
            return;
        }
        let (lost, released, moved) = ctx.input(|i| {
            let lost = i.events.iter().any(|e| matches!(e, egui::Event::PointerGone | egui::Event::WindowFocused(false)));
            let released =
                i.events.iter().any(|e| matches!(e, egui::Event::PointerButton { button: egui::PointerButton::Primary, pressed: false, .. }));
            let moved = i.events.iter().rev().find_map(|e| match e {
                egui::Event::PointerMoved(p) => Some(*p),
                _ => None,
            });
            (lost, released, moved)
        });
        if !lost || released {
            return;
        }
        let scale = self.canvas_scale.max(0.05);
        let here = moved
            .zip(self.slide_rect)
            .map(|(p, r)| deckcraft_geom::Point::new(f64::from((p.x - r.min.x) / scale), f64::from((p.y - r.min.y) / scale)));
        let Some(at) = here.or(self.last_drag_at) else { return };
        let ev = |kind| deckcraft_engine::PointerEvent {
            kind,
            x: at.x,
            y: at.y,
            mods: deckcraft_engine::Mods::default(),
            tol: f64::from(4.0 / scale),
            pressure: None,
        };
        // A move, resize or rotate takes its point from Drag, not Up: move there first.
        let r = if self.last_drag_at == Some(at) {
            self.session.pointer(ev(deckcraft_engine::PointerKind::Up))
        } else {
            self.session.pointer(ev(deckcraft_engine::PointerKind::Drag)).and_then(|_| self.session.pointer(ev(deckcraft_engine::PointerKind::Up)))
        };
        if let Err(e) = r {
            self.set_status(e.to_string());
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn write_file(path: &str, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| e.to_string())
}

#[cfg(target_arch = "wasm32")]
fn write_file(_path: &str, _bytes: &[u8]) -> Result<(), String> {
    Err(tr(deckcraft_live::UNMERGED_TEXT).to_owned())
}

/// What the person did in the live strip this frame.
enum Act {
    SaveCopy,
    Restore(Tomb),
    Dismiss,
    Follow(String),
}

/// The live strip's height.
pub(crate) const STRIP_H: f32 = 30.0;

/// Whether the live strip is drawn this frame.
pub(crate) fn strip_shown(app: &SlideApp) -> bool {
    !app.pending_unmerged.is_empty() || (app.backstage.is_none() && app.live.as_ref().is_some_and(|l| l.applies(&app.session)))
}

/// The live strip above the status bar: status, the newest notice and the other editors; and,
/// while a copy of unmerged edits waits (also after the live presentation closed), its offer.
pub(crate) fn status_strip(app: &mut SlideApp, ui: &mut Ui) {
    if !strip_shown(app) {
        return;
    }
    let copy_waiting = !app.pending_unmerged.is_empty();
    let lu = app.live.as_ref().filter(|l| app.backstage.is_none() && l.applies(&app.session));
    let t = Tokens::get(ui.ctx());
    let status = lu.map(LiveUi::status_line);
    let notice = lu.and_then(|l| l.notices.last().cloned());
    let bar = lu.map(|l| l.bar.clone());
    let mut act = None;
    egui::Panel::bottom("live_status").exact_size(STRIP_H).frame(egui::Frame::NONE.fill(t.chrome).inner_margin(egui::Margin::symmetric(10, 0))).show(
        ui,
        |ui| {
            ui.horizontal_centered(|ui| {
                if let Some((line, tip)) = &status {
                    let s = ui.add(egui::Label::new(egui::RichText::new(line).size(12.0).color(t.text)).sense(Sense::hover()));
                    s.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, line));
                    if let Some(tip) = tip {
                        s.on_hover_text(tip);
                    }
                }
                if copy_waiting {
                    ui.add_space(16.0);
                    let text = tr(deckcraft_live::UNMERGED_TEXT);
                    let l = ui.add(egui::Label::new(egui::RichText::new(text).size(12.0).color(t.accent)));
                    l.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, text));
                    if ui.button(tr(SAVE_COPY_TEXT)).clicked() {
                        act = Some(Act::SaveCopy);
                    }
                }
                if let Some(n) = &notice {
                    ui.add_space(16.0);
                    let text = tr(&n.text).to_owned();
                    let l = ui.add(egui::Label::new(egui::RichText::new(&text).size(12.0).color(t.accent)));
                    l.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, &text));
                    if let Some(target) = &n.restore
                        && ui.button(tr(RESTORE_LABEL)).clicked()
                    {
                        act = Some(Act::Restore(*target));
                    }
                    if ui.small_button("×").on_hover_text(tr(DISMISS_TEXT)).clicked() {
                        act = Some(Act::Dismiss);
                    }
                }
                if let Some(bar) = bar {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(id) = bar.badges_only().show(ui) {
                            act = Some(Act::Follow(id));
                        }
                    });
                }
            });
        },
    );
    match act {
        Some(Act::SaveCopy) => app.offer_unmerged(),
        Some(Act::Restore(target)) => {
            let Some(lu) = app.live.as_mut() else { return };
            lu.notices.pop();
            if let Err(e) = lu.restore(&mut app.session, target) {
                app.set_status(e);
            }
        }
        Some(Act::Dismiss) => {
            if let Some(lu) = app.live.as_mut() {
                lu.notices.pop();
            }
        }
        Some(Act::Follow(id)) => {
            let Some(peer) = app.live.as_ref().and_then(|lu| lu.peers.iter().find(|p| p.id == id)).cloned() else { return };
            // No place to go: the slide they were on was deleted (their caret is dropped until
            // they select again, L7), or they have selected nothing yet. Say so; nothing broke.
            let Some(at) = peer.caret.filter(|c| c.slide.is_some()) else {
                app.set_status(fill(NO_PLACE_TEXT, &[("name", &peer.display)]));
                return;
            };
            if let Some(slide) = at.slide {
                let _ = app.run("slide.go", json!({"index": slide}));
                if !at.shapes.is_empty() {
                    let ids: Vec<u32> = at.shapes.iter().map(|s| s.0).collect();
                    let _ = app.run("edit.select", json!({"ids": ids}));
                }
            }
        }
        None => {}
    }
}

/// Replaces `{key}` placeholders.
pub(crate) fn fill(key: &str, with: &[(&str, &str)]) -> String {
    let mut s = tr(key).to_owned();
    for (k, v) in with {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

/// An accessibility node over `rect` (hover only, so the canvas keeps its clicks), answering
/// whether the pointer is on it.
fn named_area(ui: &Ui, rect: Rect, id: egui::Id, name: &str) -> bool {
    let resp = ui.interact(rect, id, Sense::hover());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Image, true, name));
    resp.hovered()
}

/// The other editors' selections and carets on the slide shown.
pub(crate) fn canvas_marks(app: &mut SlideApp, ui: &Ui, painter: &egui::Painter, xf: Xf) {
    let Some(lu) = app.live.as_mut().filter(|l| l.applies(&app.session)) else { return };
    let Some(st) = app.session.active() else { return };
    let here = st.selection.slide;
    let now = ui.input(|i| i.time);
    let mut wake: Option<f64> = None;
    let peers = lu.peers.clone();
    for peer in &peers {
        let Some(at) = peer.caret.as_ref().filter(|p| p.slide == Some(here)) else { continue };
        let colour = lu.colour(peer);
        let stroke = Stroke::new(2.0, colour);
        let mut sig = std::collections::hash_map::DefaultHasher::new();
        let mut outline: Option<Rect> = None;
        let mut hovered = false;
        for id in &at.shapes {
            let Some(sh) = st.shape(*id) else { continue };
            let x = deckcraft_engine::cmd::xfrm_of(&st.doc, &st.selection, sh);
            std::hash::Hash::hash(&(id.0, x.x.to_bits(), x.y.to_bits(), x.w.to_bits(), x.h.to_bits(), x.rot.to_bits()), &mut sig);
            let a = x.affine();
            let c = [Point::new(0.0, 0.0), Point::new(x.w, 0.0), Point::new(x.w, x.h), Point::new(0.0, x.h)].map(|q| xf.to_screen(a * q));
            for i in 0..4 {
                painter.line_segment([c[i], c[(i + 1) % 4]], stroke);
            }
            let r = Rect::from_points(&c).expand(2.0);
            outline = Some(outline.map_or(r, |o| o.union(r)));
            let name = fill(SELECTION_NAME, &[("name", &peer.display), ("shape", &sh.name), ("slide", &(here + 1).to_string())]);
            hovered |= named_area(ui, r, ui.scope_id().with(("live_sel", &peer.id, id.0)), &name);
        }
        if let Some(c) = &at.caret
            && let TextTarget::Shape(id) = c.target
            && let Some(sh) = st.shape(id)
        {
            let ts = TextSel { shape: id, anchor: c.anchor, caret: c.head, ..Default::default() };
            if let Some((p0, p1)) = deckcraft_engine::cmd::text::caret_line(st, &ts) {
                let (a, b) = (xf.to_screen(p0), xf.to_screen(p1));
                painter.line_segment([a, b], stroke);
                let name = fill(CURSOR_NAME, &[("name", &peer.display), ("shape", &sh.name)]);
                named_area(ui, Rect::from_two_pos(a, b).expand(3.0), ui.scope_id().with(("live_caret", &peer.id)), &name);
            }
        }
        let Some(outline) = outline else { continue };
        let sig = std::hash::Hasher::finish(&sig);
        let since = match lu.moved.get(&peer.id) {
            Some((s, t)) if *s == sig => *t,
            _ => {
                lu.moved.insert(peer.id.clone(), (sig, now));
                now
            }
        };
        let left = TAG_SECS - (now - since);
        if left > 0.0 || hovered {
            name_tag(painter, pos2(outline.max.x, outline.min.y), &peer.display, colour);
        }
        if left > 0.0 {
            wake = Some(wake.map_or(left, |w: f64| w.min(left)));
        }
    }
    if let Some(w) = wake {
        ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(w.max(0.05)));
    }
}

/// A name in the editor's colour, its bottom right at `corner` (an outline's top right).
fn name_tag(painter: &egui::Painter, corner: Pos2, name: &str, colour: Color32) {
    let galley = painter.layout_no_wrap(name.to_owned(), theme::font(11.0), Color32::WHITE);
    let r = Rect::from_min_size(pos2(corner.x - galley.size().x - 8.0, corner.y - galley.size().y - 4.0), galley.size() + vec2(8.0, 4.0));
    painter.rect_filled(r, CornerRadius::same(3), colour);
    painter.galley(r.min + vec2(4.0, 2.0), galley, Color32::WHITE);
}

/// A dot per editor on slide `index`'s thumbnail `img`.
pub(crate) fn slide_dots(app: &SlideApp, ui: &Ui, index: usize, img: Rect) {
    let Some(lu) = app.live.as_ref().filter(|l| l.applies(&app.session)) else { return };
    let mut x = img.max.x - 7.0;
    for peer in lu.peers.iter().filter(|p| p.caret.as_ref().is_some_and(|c| c.slide == Some(index))) {
        let c = pos2(x, img.min.y + 7.0);
        ui.painter().circle(c, 4.5, lu.colour(peer), Stroke::new(1.0, Color32::WHITE));
        let name = fill(ON_SLIDE_NAME, &[("name", &peer.display), ("slide", &(index + 1).to_string())]);
        named_area(ui, Rect::from_center_size(c, vec2(10.0, 10.0)), ui.scope_id().with(("live_dot", &peer.id, index)), &name);
        x -= 11.0;
    }
}
