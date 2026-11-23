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
}

/// The live presentation as the window holds it.
pub type LiveDeck = DeckLive<BoxHub>;

/// The button on a notice about something deleted. L5 adds `deckcraft_live::RESTORE_LABEL`
/// with the same text; this one goes when that lands.
pub const RESTORE_LABEL: &str = "Restore";
/// The live strip's status for a person with a read-only ticket.
pub const VIEWING_TEXT: &str = "Viewing";
/// The live strip's status while connected and editing.
pub const LIVE_TEXT: &str = "Editing live";
/// Closes a notice.
pub const DISMISS_TEXT: &str = "Dismiss";
/// The accessible name of one editor's selected shape: `{name}`, `{shape}`, `{slide}`.
pub const SELECTION_NAME: &str = "{name}'s selection, {shape} on slide {slide}";
/// The accessible name of one editor's text caret: `{name}`, `{shape}`.
pub const CURSOR_NAME: &str = "{name}'s cursor in {shape}";
/// The accessible name of the dot on a slide an editor is on: `{name}`, `{slide}`.
pub const ON_SLIDE_NAME: &str = "{name} on slide {slide}";

/// Every English key this module draws, for the translation report.
pub const ENGLISH_KEYS: &[&str] = &[RESTORE_LABEL, VIEWING_TEXT, LIVE_TEXT, DISMISS_TEXT, SELECTION_NAME, CURSOR_NAME, ON_SLIDE_NAME];

/// The translation lookup for an English key. DeckCraft has no translation catalogue yet, so this
/// is English as it stands; every live line goes through it so a catalogue needs one change.
#[must_use]
pub fn tr(key: &str) -> &str {
    key
}

thread_local! {
    /// Why the command the ribbon asked about last ([`crate::ribbon::enabled`]) is greyed, for
    /// the widget drawn for it next.
    static GATED: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

/// Notes why the command just asked about is greyed (None when it is not).
pub(crate) fn note_gate(why: Option<&'static str>) {
    GATED.set(why);
}

/// Gives a greyed ribbon control the live reason as its tooltip, when the live gate greyed it.
pub(crate) fn gate_hint(resp: egui::Response, enabled: bool) -> egui::Response {
    match GATED.take() {
        Some(why) if !enabled => resp.on_hover_text(tr(why)),
        _ => resp,
    }
}

/// A menu entry for engine command `id`, greyed when it cannot run, with the live reason as its
/// tooltip when the live gate greyed it.
pub(crate) fn menu_button(app: &SlideApp, ui: &mut Ui, label: &str, id: &str) -> egui::Response {
    let r = ui.add_enabled(crate::ribbon::enabled(app, id), egui::Button::new(label));
    match app.live_refusal(id) {
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
        LiveUi {
            live,
            peers: Vec::new(),
            colours: HashMap::new(),
            bar: PresenceBar::new(),
            notices: Vec::new(),
            sent: None,
            moved: HashMap::new(),
        }
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
    pub fn pump(&mut self, session: &mut Session) {
        self.peers = self.live.pump(session);
        let ids: Vec<String> = self.peers.iter().map(|p| p.id.clone()).collect();
        self.colours = ids.iter().cloned().zip(colours_for(&ids)).collect();
        self.bar.set(self.peers.iter().map(|p| Badge::new(p.id.clone(), p.display.clone())).collect());
        self.moved.retain(|id, _| ids.contains(id));
        self.take_notices();
        let key = session.documents().iter().find(|d| d.uid == self.live.doc()).map(|st| {
            let sel = &st.selection;
            (sel.slide, sel.shapes.clone(), sel.text.as_ref().map(|t| (t.shape, t.anchor, t.caret, t.notes)))
        });
        if key.is_some() && key != self.sent {
            self.live.send_selection(session);
            self.sent = key;
        }
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
        let sections = session.documents().iter().find(|d| d.uid == self.live.doc()).is_some_and(|d| !d.doc.sections.is_empty() || !d.doc.custom_shows.is_empty());
        if sections && SLIDE_MOVES.contains(&engine) {
            return Err(SECTIONS_TEXT);
        }
        self.live.allows(engine)
    }

    /// Why typing is not taken now, when it is not.
    #[must_use]
    pub fn typing_refused(&self) -> Option<&'static str> {
        self.live.allows("text.insert").err()
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
    /// Hands the window a presentation the host opened live on this window's session.
    pub fn start_live(&mut self, live: LiveDeck) {
        self.live = Some(LiveUi::new(live));
    }

    /// Takes a live session the host left in [`crate::Services`], drops one whose presentation was
    /// closed (so this window leaves the room at once), and pumps the rest.
    pub(crate) fn pump_live(&mut self, ctx: &egui::Context) {
        if let Some(l) = self.services.live.take() {
            self.start_live(l);
        }
        let Some(lu) = self.live.as_mut() else { return };
        if !lu.open(&self.session) {
            self.live = None;
            return;
        }
        lu.pump(&mut self.session);
        // Edits and carets from the room arrive without input: keep frames coming.
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }

    /// Why `id` is greyed now, when the live gate refuses it.
    pub(crate) fn live_refusal(&self, id: &str) -> Option<&'static str> {
        let lu = self.live.as_ref().filter(|l| l.applies(&self.session))?;
        lu.gate(&self.session, id).err()
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

    /// A drag whose release never came (the window lost focus, the pointer was cancelled or left)
    /// ends where it last was: Esc cannot cancel a move drag, and a drag left open would hold the
    /// other editors' changes back (L4 concern 2, D4 review M3).
    pub(crate) fn end_lost_drag(&mut self, ctx: &egui::Context) {
        if !self.session.tool.dragging() {
            return;
        }
        let lost = ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::PointerGone | egui::Event::WindowFocused(false))));
        if !lost {
            return;
        }
        let at = self.last_drag_at.unwrap_or_default();
        let ev = deckcraft_engine::PointerEvent {
            kind: deckcraft_engine::PointerKind::Up,
            x: at.x,
            y: at.y,
            mods: deckcraft_engine::Mods::default(),
            tol: f64::from(4.0 / self.canvas_scale.max(0.05)),
            pressure: None,
        };
        if let Err(e) = self.session.pointer(ev) {
            self.set_status(e.to_string());
        }
    }
}

/// What the person did in the live strip this frame.
enum Act {
    Restore(Tomb),
    Dismiss,
    Follow(String),
}

/// The live strip above the status bar: status, the newest notice and the other editors.
pub(crate) fn status_strip(app: &mut SlideApp, ui: &mut Ui) {
    let Some(lu) = app.live.as_mut().filter(|l| l.applies(&app.session)) else { return };
    let t = Tokens::get(ui.ctx());
    let (line, tip) = lu.status_line();
    let notice = lu.notices.last().cloned();
    let mut act = None;
    egui::Panel::bottom("live_status").exact_size(30.0).frame(egui::Frame::NONE.fill(t.chrome).inner_margin(egui::Margin::symmetric(10, 0))).show(ui, |ui| {
        ui.horizontal_centered(|ui| {
            let status = ui.add(egui::Label::new(egui::RichText::new(&line).size(12.0).color(t.text)).sense(Sense::hover()));
            status.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, &line));
            if let Some(tip) = &tip {
                status.on_hover_text(tip);
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
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(id) = lu.bar.clone().badges_only().show(ui) {
                    act = Some(Act::Follow(id));
                }
            });
        });
    });
    match act {
        Some(Act::Restore(target)) => {
            lu.notices.pop();
            if let Err(e) = lu.restore(&mut app.session, target) {
                app.set_status(e);
            }
        }
        Some(Act::Dismiss) => {
            lu.notices.pop();
        }
        Some(Act::Follow(id)) => {
            let Some(at) = lu.peers.iter().find(|p| p.id == id).and_then(|p| p.caret.clone()) else { return };
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
fn fill(key: &str, with: &[(&str, &str)]) -> String {
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
            hovered |= named_area(ui, r, ui.id().with(("live_sel", &peer.id, id.0)), &name);
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
                named_area(ui, Rect::from_two_pos(a, b).expand(3.0), ui.id().with(("live_caret", &peer.id)), &name);
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
        named_area(ui, Rect::from_center_size(c, vec2(10.0, 10.0)), ui.id().with(("live_dot", &peer.id, index)), &name);
        x -= 11.0;
    }
}
