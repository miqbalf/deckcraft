//! The live interface in the window: a real room on craft-live's in-memory relay, with this
//! window (Ann) in a kittest harness and a second editor (Bob) as a bare engine session.
//! Test decks are built in code through registered commands and saved as `.pptx` (rule 24).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use craft_live::testing::{MemHub, MemRelay, ROOM};
use craft_live::{Access, LiveStatus};
use deckcraft_engine::{Mods, PointerEvent, PointerKind, Session};
use deckcraft_live::{BaseFile, DeckLive, MEDIA_TEXT, NOT_YET_TEXT, READ_ONLY_TEXT, SECTIONS_TEXT};
use deckcraft_model::{Presentation, ShapeId, SlideId};
use egui::accesskit::Role;
use egui::{Color32, Rect};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use serde_json::{Value, json};

use super::live::{BoxHub, RESTORE_LABEL, VIEWING_TEXT};
use super::{Services, SlideApp};

const NAME: &str = "Deck.pptx";

/// Three slides; the second holds the boxes "alpha" and "beta", the third "gamma".
fn deck(sections: bool) -> Vec<u8> {
    let mut s = Session::with_new();
    s.execute("slide.new", &json!({"layout": "blank"})).unwrap();
    s.execute("shape.insert", &json!({"preset": "rect", "rect": [60.0, 60.0, 200.0, 80.0], "text": "alpha"})).unwrap();
    s.execute("shape.insert", &json!({"preset": "rect", "rect": [320.0, 200.0, 200.0, 80.0], "text": "beta"})).unwrap();
    s.execute("slide.new", &json!({"layout": "blank"})).unwrap();
    s.execute("shape.insert", &json!({"preset": "rect", "rect": [60.0, 260.0, 200.0, 80.0], "text": "gamma"})).unwrap();
    if sections {
        s.execute("slide.go", &json!({"index": 0})).unwrap();
        s.execute("section.add", &json!({"name": "Intro"})).unwrap();
    }
    s.execute("slide.go", &json!({"index": 0})).unwrap();
    deckcraft_engine::cmd::file::save_bytes(&s.active().unwrap().doc, "pptx").unwrap()
}

/// The shape whose text is `text`.
fn find(p: &Presentation, text: &str) -> ShapeId {
    let mut found = None;
    for s in &p.slides {
        deckcraft_model::walk(&s.shapes, &mut |sh, _| {
            if found.is_none() && sh.text.as_ref().is_some_and(|t| t.text() == text) {
                found = Some(sh.id);
            }
        });
    }
    found.unwrap()
}

fn shape_name(p: &Presentation, id: ShapeId) -> String {
    p.slides.iter().find_map(|s| s.shape(id)).unwrap().name.clone()
}

struct Bob {
    live: DeckLive<MemHub>,
    session: Session,
}

impl Bob {
    fn doc(&self) -> Arc<Presentation> {
        let uid = self.live.doc();
        self.session.documents().iter().find(|d| d.uid == uid).unwrap().doc.clone()
    }

    fn run(&mut self, id: &str, params: Value) -> Value {
        self.live.run(&mut self.session, id, &params).unwrap()
    }
}

struct Room {
    relay: MemRelay,
    /// Ann's link, for dropping it.
    ann_hub: MemHub,
    h: Harness<'static, SlideApp>,
    bob: Bob,
    clock: Arc<AtomicU64>,
}

impl Room {
    /// Ann in the window and Bob beside her, with the given tickets, on `bytes`.
    fn new(bytes: &[u8], ann: Access, bob: Access) -> Room {
        Room::with_services(bytes, ann, bob, Services::default())
    }

    fn with_services(bytes: &[u8], ann: Access, bob: Access, services: Services) -> Room {
        let relay = MemRelay::new();
        let clock = Arc::new(AtomicU64::new(0));
        let base = BaseFile { bytes, name: NAME, expected_sha256: None };

        let hub = relay.connect("ann", "Ann", ann == Access::Edit);
        let ann_hub = hub.clone();
        let mut session = Session::new();
        let mut live = DeckLive::start(BoxHub(Box::new(hub)), ROOM, &base, ann, "ann", &mut session).unwrap();
        let c = clock.clone();
        live.set_clock(Box::new(move || c.load(Ordering::Relaxed) as f64));
        let app = SlideApp::new(session, Services { live: Some(live), ..services });
        let h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_ui_state(
            |ui, app: &mut SlideApp| {
                app.logic(ui.ctx());
                app.ui(ui);
            },
            app,
        );

        let hub = relay.connect("bob", "Bob", bob == Access::Edit);
        let mut session = Session::new();
        let mut live = DeckLive::start(hub, ROOM, &base, bob, "bob", &mut session).unwrap();
        let c = clock.clone();
        live.set_clock(Box::new(move || c.load(Ordering::Relaxed) as f64));

        let mut room = Room { relay, ann_hub, h, bob: Bob { live, session }, clock };
        room.settle();
        room
    }

    fn editors(bytes: &[u8]) -> Room {
        Room::new(bytes, Access::Edit, Access::Edit)
    }

    /// Both sides pump (and the window draws) until carets and edits have crossed.
    fn settle(&mut self) {
        for _ in 0..12 {
            self.clock.fetch_add(250, Ordering::Relaxed);
            self.bob.live.pump(&mut self.bob.session);
            self.bob.live.send_selection(&self.bob.session);
            self.h.run_steps(1);
        }
    }

    fn ann(&self) -> &SlideApp {
        self.h.state()
    }

    fn ann_doc(&self) -> Arc<Presentation> {
        self.ann().session.active().unwrap().doc.clone()
    }

    fn ann_slide(&self) -> SlideId {
        let st = self.ann().session.active().unwrap();
        st.doc.slides[st.selection.slide].id
    }
}

/// Every painted shape, nested lists opened.
fn painted(h: &Harness<'_, SlideApp>) -> Vec<egui::Shape> {
    fn open(s: &egui::Shape, out: &mut Vec<egui::Shape>) {
        match s {
            egui::Shape::Vec(v) => v.iter().for_each(|s| open(s, out)),
            s => out.push(s.clone()),
        }
    }
    let mut out = Vec::new();
    for c in &h.output().shapes {
        open(&c.shape, &mut out);
    }
    out
}

/// Colours of the filled rectangles and circles painted inside `r`.
fn fills_inside(h: &Harness<'_, SlideApp>, r: Rect) -> Vec<Color32> {
    painted(h)
        .into_iter()
        .filter_map(|s| match s {
            egui::Shape::Rect(x) if r.expand(2.0).contains_rect(x.rect) && x.fill.a() > 0 => Some(x.fill),
            egui::Shape::Circle(c) if r.expand(2.0).contains(c.center) && c.fill.a() > 0 => Some(c.fill),
            _ => None,
        })
        .collect()
}

/// Colours of the lines painted along `r` (both ends inside it, slightly grown).
fn lines_inside(h: &Harness<'_, SlideApp>, r: Rect) -> Vec<Color32> {
    painted(h)
        .into_iter()
        .filter_map(|s| match s {
            egui::Shape::LineSegment { points, stroke } if points.iter().all(|p| r.expand(3.0).contains(*p)) => Some(stroke.color),
            _ => None,
        })
        .collect()
}

fn node_rect(h: &Harness<'_, SlideApp>, role: Role, label: &str) -> Rect {
    h.get_by_role_and_label(role, label).rect()
}

fn label_rect(h: &Harness<'_, SlideApp>, label: &str) -> Rect {
    h.get_by_label(label).rect()
}

#[test]
fn remote_selection_is_drawn_with_a_named_accessibility_node() {
    let mut room = Room::editors(&deck(false));
    let doc = room.bob.doc();
    let beta = find(&doc, "beta");
    room.h.state_mut().run("slide.go", json!({"index": 1})).unwrap();
    room.bob.run("slide.go", json!({"index": 1}));
    room.bob.run("edit.select", json!({"ids": [beta.0]}));
    room.settle();
    let name = shape_name(&doc, beta);
    room.h.get_by_label(&format!("Bob's selection, {name} on slide 2"));

    room.bob.run("text.edit", json!({"id": beta.0}));
    room.settle();
    room.h.get_by_label(&format!("Bob's cursor in {name}"));
}

#[test]
fn badge_outline_and_thumbnail_dot_share_a_colour() {
    let mut room = Room::editors(&deck(false));
    let doc = room.bob.doc();
    let alpha = find(&doc, "alpha");
    room.h.state_mut().run("slide.go", json!({"index": 1})).unwrap();
    room.bob.run("slide.go", json!({"index": 1}));
    room.bob.run("edit.select", json!({"ids": [alpha.0]}));
    room.settle();

    let badge = fills_inside(&room.h, node_rect(&room.h, Role::Button, "Bob"));
    let name = shape_name(&doc, alpha);
    let outline = lines_inside(&room.h, label_rect(&room.h, &format!("Bob's selection, {name} on slide 2")));
    let dot = fills_inside(&room.h, label_rect(&room.h, "Bob on slide 2"));
    let colour = *badge.first().expect("the badge is filled");
    assert!(outline.len() >= 4 && outline.iter().all(|c| *c == colour), "outline {outline:?} is the badge's {colour:?}");
    assert!(dot.contains(&colour), "dot {dot:?} is the badge's {colour:?}");
}

#[test]
fn clicking_a_badge_goes_to_the_peer_slide() {
    let mut room = Room::editors(&deck(false));
    let doc = room.bob.doc();
    let gamma = find(&doc, "gamma");
    room.bob.run("slide.go", json!({"index": 2}));
    room.bob.run("edit.select", json!({"ids": [gamma.0]}));
    room.settle();
    assert_ne!(room.ann_slide(), doc.slides[2].id, "Ann starts elsewhere");

    room.h.get_by_role_and_label(Role::Button, "Bob").click();
    room.h.run_steps(2);
    assert_eq!(room.ann_slide(), doc.slides[2].id, "Ann is on Bob's slide");
    assert_eq!(room.ann().session.active().unwrap().selection.shapes, vec![gamma], "and has Bob's shapes selected");
}

#[test]
fn gated_command_is_greyed_with_the_reason() {
    let mut room = Room::editors(&deck(false));
    room.h.state_mut().run("view.tab", json!({"tab": "Insert"})).unwrap();
    room.h.run_steps(2);
    for (label, why) in [("Table", NOT_YET_TEXT), ("Pictures", MEDIA_TEXT)] {
        let node = room.h.get_by_role_and_label(Role::Button, label);
        assert!(node.accesskit_node().is_disabled(), "{label} is greyed");
        node.hover();
        room.h.run_steps(40);
        room.h.get_by_label_contains(why);
    }
    let before = room.ann_doc();
    assert!(room.h.state_mut().run("insert.table", json!({"rows": 2, "cols": 2})).is_err());
    assert!(Arc::ptr_eq(&before, &room.ann_doc()) || *before == *room.ann_doc(), "nothing changed");
}

#[test]
fn slide_moves_in_a_deck_with_sections_are_greyed_with_why() {
    let mut room = Room::editors(&deck(true));
    room.h.run_steps(2);
    let node = room.h.get_by_role_and_label(Role::Button, "New Slide");
    assert!(node.accesskit_node().is_disabled(), "New Slide is greyed");
    node.hover();
    room.h.run_steps(40);
    room.h.get_by_label_contains(SECTIONS_TEXT);
}

#[test]
fn restore_button_restores_the_shape() {
    let mut room = Room::editors(&deck(false));
    let doc = room.bob.doc();
    let beta = find(&doc, "beta");
    room.h.state_mut().run("slide.go", json!({"index": 1})).unwrap();
    room.h.state_mut().run("edit.select", json!({"ids": [beta.0]})).unwrap();
    room.settle();
    room.bob.run("slide.go", json!({"index": 1}));
    room.bob.run("edit.delete", json!({"ids": [beta.0]}));
    room.settle();
    assert!(room.ann_doc().slides[1].shape(beta).is_none(), "Bob's delete reached Ann");
    room.h.get_by_label(&deckcraft_live::deleted_text("Bob"));
    room.h.get_by_role_and_label(Role::Button, RESTORE_LABEL).click();
    room.settle();
    assert!(room.ann_doc().slides[1].shape(beta).is_some(), "back for Ann");
    assert!(room.bob.doc().slides[1].shape(beta).is_some(), "back for Bob");
}

#[test]
fn read_only_blocks_typing_and_says_so() {
    let mut room = Room::new(&deck(false), Access::Read, Access::Edit);
    let alpha = find(&room.ann_doc(), "alpha");
    room.h.state_mut().run("slide.go", json!({"index": 1})).unwrap();
    room.h.state_mut().run("edit.select", json!({"ids": [alpha.0]})).unwrap();
    room.h.run_steps(1);
    let before = room.ann_doc();
    room.h.input_mut().events.push(egui::Event::Text("x".into()));
    room.h.run_steps(2);
    room.settle();
    assert!(*before == *room.ann_doc(), "nothing was typed");
    assert!(room.bob.doc().slides[1].shape(alpha).unwrap().text.as_ref().unwrap().text() == "alpha", "nothing reached Bob");
    room.h.get_by_label_contains(READ_ONLY_TEXT);
}

#[test]
fn viewer_is_labelled_viewing() {
    let mut room = Room::new(&deck(false), Access::Read, Access::Edit);
    room.h.run_steps(1);
    room.h.get_by_label(VIEWING_TEXT);
}

/// The label of a view-only member's badge in the presence list.
fn viewing(name: &str) -> String {
    super::live::fill(super::live::PEER_VIEWING_TEXT, &[("name", name)])
}

/// A member whose ticket only views is labelled "viewing" in the presence list, as the relay
/// says (C13, `Peer.access`).
#[test]
fn a_view_only_peer_is_labelled_viewing() {
    let mut room = Room::new(&deck(false), Access::Edit, Access::Read);
    room.settle();
    room.h.get_by_role_and_label(Role::Button, &viewing("Bob"));
    assert!(room.h.query_by_role_and_label(Role::Button, "Bob").is_none(), "not without the label");
}

/// An editor's badge carries no label.
#[test]
fn an_editor_is_not_labelled() {
    let mut room = Room::editors(&deck(false));
    room.settle();
    room.h.get_by_role_and_label(Role::Button, "Bob");
    assert!(room.h.query_by_role_and_label(Role::Button, &viewing("Bob")).is_none());
}

/// An older relay sends no access in its roster: everyone reads as an editor, so nobody is
/// labelled, as before C13.
#[test]
fn a_peer_from_an_old_relay_is_not_labelled() {
    let mut room = Room::new(&deck(false), Access::Edit, Access::Read);
    room.settle();
    room.h.get_by_role_and_label(Role::Button, &viewing("Bob"));
    let entry = |hub: &MemHub, name: &str| {
        serde_json::from_value::<craft_collab::Presence>(json!({"peer": room.relay.peer_id(hub), "display": name})).expect("an old roster entry")
    };
    let roster = vec![entry(&room.ann_hub, "Ann"), entry(room.bob.live.core().hub(), "Bob")];
    room.relay.inject(&room.ann_hub, craft_collab::ServerMsg::Presence { doc: ROOM.into(), roster });
    room.h.run_steps(2);
    room.h.get_by_role_and_label(Role::Button, "Bob");
    assert!(room.h.query_by_role_and_label(Role::Button, &viewing("Bob")).is_none(), "no access read as edit");
}

#[test]
fn the_canvas_stays_on_my_slide_when_a_peer_reorders() {
    let mut room = Room::editors(&deck(false));
    room.h.state_mut().run("slide.go", json!({"index": 1})).unwrap();
    room.h.run_steps(1);
    let mine = room.ann_slide();
    room.bob.run("slide.move", json!({"ids": [mine.0], "to": 2}));
    room.settle();
    assert_eq!(room.ann_doc().slides.iter().position(|s| s.id == mine), Some(2), "Bob's move reached Ann");
    assert_eq!(room.ann_slide(), mine, "Ann is still on her slide");
    room.h.get_by_label("Slide 3 of 3");
}

/// While live, Undo and Redo take back only my own commands (L6, Ruling D4): they are offered only
/// when there is one, never because a peer edited (D4 review M5, from `DeckLive::can_undo`).
#[test]
fn undo_is_offered_only_for_my_own_commands() {
    let mut room = Room::editors(&deck(false));
    let alpha = find(&room.ann_doc(), "alpha");
    room.h.run_steps(1);
    assert!(!crate::ribbon::enabled(room.ann(), "edit.undo"), "nothing of mine to undo");
    room.bob.run("slide.go", json!({"index": 1}));
    room.bob.run("arrange.nudge", json!({"ids": [alpha.0], "dx": 10.0, "dy": 0.0}));
    room.settle();
    assert!(!crate::ribbon::enabled(room.ann(), "edit.undo"), "a peer's edit is not mine to undo");
    let before = room.ann_doc();
    room.h.state_mut().run("slide.new", json!({"layout": "blank"})).unwrap();
    room.settle();
    assert!(crate::ribbon::enabled(room.ann(), "edit.undo"), "my command can be taken back");
    assert!(!crate::ribbon::enabled(room.ann(), "edit.redo"), "nothing undone yet");
    room.h.state_mut().run("edit.undo", json!({})).unwrap();
    room.settle();
    assert_eq!(room.ann_doc().slides.len(), before.slides.len(), "my slide is gone again");
    assert!(crate::ribbon::enabled(room.ann(), "edit.redo"), "and can be brought back");
}

#[test]
fn a_lost_pointer_up_ends_the_drag() {
    let mut room = Room::editors(&deck(false));
    let alpha = find(&room.ann_doc(), "alpha");
    room.h.state_mut().run("slide.go", json!({"index": 1})).unwrap();
    room.h.run_steps(1);
    let ev = |kind, x, y| PointerEvent { kind, x, y, mods: Mods::default(), tol: 4.0, pressure: None };
    let s = &mut room.h.state_mut().session;
    s.pointer(ev(PointerKind::Down, 100.0, 100.0)).unwrap();
    s.pointer(ev(PointerKind::Drag, 140.0, 120.0)).unwrap();
    // Where the canvas records the drag as it goes (this test drives the session directly).
    room.h.state_mut().last_drag_at = Some(deckcraft_geom::Point::new(140.0, 120.0));
    assert!(room.h.state().session.tool.dragging(), "a move drag is open");
    room.h.input_mut().events.push(egui::Event::WindowFocused(false));
    room.h.run_steps(1);
    assert!(!room.h.state().session.tool.dragging(), "losing the window ended it");
    room.settle();
    let moved = room.bob.doc().slides[1].shape(alpha).unwrap().xfrm.unwrap();
    let was = room.ann_doc();
    assert!(*room.bob.doc() == *was && moved.x > 60.0, "the drop was shared");
}

/// A hub that says when the window let it go: a real hub closes its link when dropped, which
/// is how the other editors see this window leave (MemRelay does not model a dropped hub).
struct Watched(MemHub, Arc<std::sync::atomic::AtomicBool>);

impl craft_collab::Hub for Watched {
    fn send(&mut self, msg: craft_collab::ClientMsg) -> craft_collab::Result<()> {
        self.0.send(msg)
    }
    fn poll(&mut self) -> Vec<craft_collab::ServerMsg> {
        self.0.poll()
    }
    fn connected(&self) -> bool {
        self.0.connected()
    }
    fn set_space(&mut self, space: u32) {
        self.0.set_space(space);
    }
    fn close(&mut self) {
        self.0.close();
    }
}

impl Drop for Watched {
    fn drop(&mut self) {
        self.1.store(true, Ordering::Relaxed);
    }
}

#[test]
fn closing_the_live_presentation_leaves_the_room() {
    let bytes = deck(false);
    let relay = MemRelay::new();
    let gone = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hub = Watched(relay.connect("ann", "Ann", true), gone.clone());
    let mut session = Session::new();
    let base = BaseFile { bytes: &bytes, name: NAME, expected_sha256: None };
    let live = DeckLive::start(BoxHub(Box::new(hub)), ROOM, &base, Access::Edit, "ann", &mut session).unwrap();
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_ui_state(
        |ui, app: &mut SlideApp| {
            app.logic(ui.ctx());
            app.ui(ui);
        },
        SlideApp::new(session, Services { live: Some(live), ..Default::default() }),
    );
    h.run_steps(3);
    assert!(h.state().live.is_some() && !gone.load(Ordering::Relaxed), "live while open");
    let _ = h.state_mut().run("file.close", json!({}));
    h.run_steps(2);
    assert!(h.state().live.is_none(), "the window let the live session go");
    assert!(gone.load(Ordering::Relaxed), "and closed its link to the room");
}

/// Records what the host was handed: downloads, or picks and writes.
#[derive(Clone, Default)]
struct HostLog(std::rc::Rc<std::cell::RefCell<Vec<(String, Vec<u8>)>>>);

impl HostLog {
    fn got(&self) -> Vec<(String, Vec<u8>)> {
        self.0.borrow().clone()
    }
}

/// A web host: every copy is a download.
fn downloading(log: &HostLog) -> Services {
    let l = log.clone();
    Services { download: Some(Box::new(move |name: &str, b: &[u8]| l.0.borrow_mut().push((name.to_owned(), b.to_vec())))), ..Default::default() }
}

/// A desktop host whose Save dialog the person cancels.
fn cancelling() -> Services {
    Services { pick_save: Some(Box::new(|_: &str| None)), ..Default::default() }
}

fn copy() -> (String, Vec<u8>) {
    ("Deck (unmerged 2026-11-23 1404).pptx".to_owned(), deck(false))
}

#[test]
fn an_unmerged_copy_is_offered_and_saved() {
    let log = HostLog::default();
    let mut room = Room::with_services(&deck(false), Access::Edit, Access::Edit, downloading(&log));
    room.h.state_mut().pending_unmerged.push(copy());
    room.h.run_steps(2);
    room.h.get_by_label(deckcraft_live::UNMERGED_TEXT);
    room.h.get_by_role_and_label(Role::Button, super::live::SAVE_COPY_TEXT).click();
    room.h.run_steps(2);
    assert_eq!(log.got(), vec![copy()], "the host got the copy");
    assert!(room.h.state().pending_unmerged.is_empty(), "nothing left to save");
    assert!(room.h.query_by_label(deckcraft_live::UNMERGED_TEXT).is_none(), "the offer is gone");
}

/// DC1: a copy kept because the offline buffer filled is offered with its own line, not as
/// changes that "could not be merged".
#[test]
fn a_full_buffer_copy_has_its_own_line_in_the_strip() {
    let log = HostLog::default();
    let mut room = Room::with_services(&deck(false), Access::Edit, Access::Edit, downloading(&log));
    room.h.state_mut().keep_copy(copy(), true);
    room.h.run_steps(2);
    room.h.get_by_label(super::live::FULL_COPY_TEXT);
    assert!(room.h.query_by_label(deckcraft_live::UNMERGED_TEXT).is_none(), "not told as unmerged");
    // A copy the room could not merge waits too: the strip says the general line.
    room.h.state_mut().keep_copy(("Deck (unmerged 2026-11-23 1405).pptx".to_owned(), deck(false)), false);
    room.h.run_steps(2);
    room.h.get_by_label(deckcraft_live::UNMERGED_TEXT);
    room.h.get_by_role_and_label(Role::Button, super::live::SAVE_COPY_TEXT).click();
    room.h.run_steps(2);
    assert_eq!(log.got().len(), 1, "the oldest copy was saved");
    room.h.get_by_label(deckcraft_live::UNMERGED_TEXT);
}

/// DC1 review m8: when the live presentation ends with only a full-buffer copy waiting, the
/// window says the copy holds waiting changes, not that some could not be merged.
#[test]
fn ending_with_only_a_full_buffer_copy_says_so() {
    let mut room = Room::with_services(&deck(false), Access::Edit, Access::Edit, cancelling());
    room.h.state_mut().keep_copy(copy(), true);
    room.h.state_mut().end_live();
    let said = room.h.state().toast.as_ref().map(|t| t.text.clone()).unwrap_or_default();
    assert_eq!(said, super::live::FULL_COPY_TEXT);
}

#[test]
fn a_cancelled_save_keeps_the_copy() {
    let mut room = Room::with_services(&deck(false), Access::Edit, Access::Edit, cancelling());
    room.h.state_mut().pending_unmerged.push(copy());
    room.h.run_steps(2);
    room.h.get_by_role_and_label(Role::Button, super::live::SAVE_COPY_TEXT).click();
    room.h.run_steps(2);
    assert_eq!(room.h.state().pending_unmerged, vec![copy()], "the copy is still there");
    room.h.get_by_role_and_label(Role::Button, super::live::SAVE_COPY_TEXT);
}

/// Ann's link drops, she keeps typing (editing goes on while reconnecting), and the live
/// presentation is closed by a path that is not `run("file.close")`: what the room never got
/// comes to the host as the unmerged copy.
#[test]
fn closing_hands_the_unmerged_copy_over() {
    let log = HostLog::default();
    let mut room = Room::with_services(&deck(false), Access::Edit, Access::Edit, downloading(&log));
    let alpha = find(&room.ann_doc(), "alpha");
    room.relay.drop_link(&room.ann_hub);
    room.settle();
    let app = room.h.state_mut();
    app.run("slide.go", json!({"index": 1})).unwrap();
    app.run("text.edit", json!({"id": alpha.0})).unwrap();
    app.run("text.insert", json!({"text": "offline"})).unwrap();
    room.h.run_steps(1);
    let i = room.h.state().session.active_index().unwrap();
    room.h.state_mut().session.close_document(i);
    room.h.run_steps(2);
    assert!(room.h.state().live.is_none(), "the window left the room");
    let got = log.got();
    assert_eq!(got.len(), 1, "one copy handed over");
    let back = deckcraft_engine::cmd::file::open_presentation(&got[0].0, &got[0].1).unwrap();
    assert!(back.slides[1].shape(alpha).unwrap().text.as_ref().unwrap().text().contains("offline"), "with Ann's offline text");
}

/// The page ends the session (a `live` for another room, or less access) while Ann types offline:
/// her edits come to the page as the copy, and the page is told unsaved work waits until then
/// (Review C1, I2).
#[test]
fn a_host_end_while_offline_hands_the_copy_over() {
    let (copies, downloads) = (HostLog::default(), HostLog::default());
    let (services, saves) = answering(&copies, &downloads);
    let mut room = Room::with_services(&deck(false), Access::Edit, Access::Edit, services);
    let alpha = find(&room.ann_doc(), "alpha");
    room.relay.drop_link(&room.ann_hub);
    room.settle();
    let app = room.h.state_mut();
    app.run("slide.go", json!({"index": 1})).unwrap();
    app.run("text.edit", json!({"id": alpha.0})).unwrap();
    app.run("text.insert", json!({"text": "offline"})).unwrap();
    room.h.run_steps(1);
    assert!(room.h.state().live_unconfirmed(), "the room has not confirmed the offline text");
    room.h.state_mut().end_live_for("The live session was restarted. Reopen the file.");
    room.h.run_steps(2);
    room.h.state_mut().hand_over_copies();
    let got = copies.got();
    assert_eq!(got.len(), 1, "one copy handed over");
    let back = deckcraft_engine::cmd::file::open_presentation(&got[0].0, &got[0].1).unwrap();
    assert!(back.slides[1].shape(alpha).unwrap().text.as_ref().unwrap().text().contains("offline"), "with Ann's offline text");
    assert!(room.h.state().copies_unsaved(), "unsaved until the page answers");
    let id = sent_id(&room);
    saves.answer(crate::HostAnswer::Saved { id, name: None });
    room.h.run_steps(1);
    assert!(!room.h.state().copies_unsaved(), "stored");
}

/// A refused link (an expired ticket at the reconnect) can recover and makes no copy: edits the
/// room never confirmed still count as unsaved for the page, so closing the tab asks first
/// (U3 review I1).
#[test]
fn a_refused_link_still_counts_its_unconfirmed_edits() {
    let mut room = Room::editors(&deck(false));
    let alpha = find(&room.ann_doc(), "alpha");
    room.relay.drop_link(&room.ann_hub);
    room.settle();
    let app = room.h.state_mut();
    app.run("slide.go", json!({"index": 1})).unwrap();
    app.run("text.edit", json!({"id": alpha.0})).unwrap();
    app.run("text.insert", json!({"text": "offline"})).unwrap();
    room.h.run_steps(1);
    // The reconnect is refused for its expired ticket before any join: the refusal is read,
    // and the link stays down, so the offline text is not sent.
    room.relay.refuse(&room.ann_hub, "auth", "the ticket expired");
    room.relay.drop_link_after_read(&room.ann_hub);
    room.h.run_steps(3);
    let status = room.h.state().live.as_ref().map(|l| l.live().status().clone());
    assert!(matches!(status, Some(LiveStatus::Refused(_))), "refused: {status:?}");
    assert!(room.h.state().live_unconfirmed(), "the offline text is still unconfirmed");
    assert!(!room.h.state().copies_unsaved(), "and no copy was made of it");
}

/// After the link ended with edits the room never confirmed, they are in the copy: the page is
/// not told of unsaved work for them any more (U2 fix-1 review N1); the copy counts until stored.
#[test]
fn a_broken_link_counts_no_unconfirmed_edits() {
    let (copies, downloads) = (HostLog::default(), HostLog::default());
    let (services, saves) = answering(&copies, &downloads);
    let mut room = Room::with_services(&deck(false), Access::Edit, Access::Edit, services);
    let alpha = find(&room.ann_doc(), "alpha");
    room.relay.drop_link(&room.ann_hub);
    room.settle();
    let app = room.h.state_mut();
    app.run("slide.go", json!({"index": 1})).unwrap();
    app.run("text.edit", json!({"id": alpha.0})).unwrap();
    app.run("text.insert", json!({"text": "offline"})).unwrap();
    room.h.run_steps(1);
    assert!(room.h.state().live_unconfirmed(), "unconfirmed while reconnecting");
    room.h.state_mut().end_live_for("The live session was restarted. Reopen the file.");
    room.h.run_steps(2);
    room.h.state_mut().hand_over_copies();
    assert!(room.h.state().live.is_some(), "the broken presentation stays open");
    assert!(!room.h.state().live_unconfirmed(), "a broken link's edits are in the copy");
    assert!(room.h.state().copies_unsaved(), "the copy counts until the page stores it");
    let id = sent_id(&room);
    saves.answer(crate::HostAnswer::Saved { id, name: None });
    room.h.run_steps(1);
    assert!(!room.h.state().live_unconfirmed() && !room.h.state().copies_unsaved(), "nothing unsaved");
}

/// Quitting the window ends the live session first; a copy that could not be saved stays offered.
#[test]
fn ending_the_live_session_keeps_unsent_edits() {
    let mut room = Room::with_services(&deck(false), Access::Edit, Access::Edit, cancelling());
    let alpha = find(&room.ann_doc(), "alpha");
    room.relay.drop_link(&room.ann_hub);
    room.settle();
    let app = room.h.state_mut();
    app.run("slide.go", json!({"index": 1})).unwrap();
    app.run("text.edit", json!({"id": alpha.0})).unwrap();
    app.run("text.insert", json!({"text": "offline"})).unwrap();
    room.h.state_mut().end_live();
    assert!(room.h.state().live.is_none(), "the session ended");
    assert_eq!(room.h.state().pending_unmerged.len(), 1, "its unsent edits wait to be saved");
}

fn slide_to_screen(h: &Harness<'_, SlideApp>, x: f64, y: f64) -> egui::Pos2 {
    let app = h.state();
    let r = app.slide_rect.unwrap();
    egui::pos2(r.min.x + x as f32 * app.canvas_scale, r.min.y + y as f32 * app.canvas_scale)
}

/// A touch end delivers the release and PointerGone in one frame: the drop lands at the release.
#[test]
fn a_release_with_pointer_gone_drops_at_the_release() {
    let mut room = Room::editors(&deck(false));
    let alpha = find(&room.ann_doc(), "alpha");
    room.h.state_mut().run("slide.go", json!({"index": 1})).unwrap();
    room.h.run_steps(2);
    let from = slide_to_screen(&room.h, 160.0, 100.0);
    let to = slide_to_screen(&room.h, 260.0, 180.0);
    let mid = from + (to - from) * 0.5;
    let press =
        |pos, pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::default() };
    room.h.input_mut().events.extend([egui::Event::PointerMoved(from), press(from, true)]);
    room.h.run_steps(1);
    room.h.input_mut().events.push(egui::Event::PointerMoved(mid));
    room.h.run_steps(1);
    room.h.input_mut().events.extend([egui::Event::PointerMoved(to), press(to, false), egui::Event::PointerGone]);
    room.h.run_steps(2);
    assert!(!room.h.state().session.tool.dragging(), "the drag ended");
    let x = room.ann_doc().slides[1].shape(alpha).unwrap().xfrm.unwrap();
    // Within snapping of the release point; the previous frame's point is 50 away.
    assert!((x.x - 160.0).abs() < 5.0 && (x.y - 140.0).abs() < 5.0, "moved by the whole drag, to the release: {x:?}");
}

#[test]
fn clicking_a_shape_under_a_peer_outline_selects_it() {
    let mut room = Room::editors(&deck(false));
    let alpha = find(&room.ann_doc(), "alpha");
    room.h.state_mut().run("slide.go", json!({"index": 1})).unwrap();
    room.bob.run("slide.go", json!({"index": 1}));
    room.bob.run("edit.select", json!({"ids": [alpha.0]}));
    room.settle();
    let at = slide_to_screen(&room.h, 160.0, 100.0);
    room.h.input_mut().events.push(egui::Event::PointerMoved(at));
    room.h.run_steps(1);
    let press =
        |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::default() };
    room.h.input_mut().events.push(press(true));
    room.h.run_steps(1);
    room.h.input_mut().events.push(press(false));
    room.h.run_steps(2);
    assert_eq!(room.ann().session.active().unwrap().selection.shapes, vec![alpha], "the click reached the canvas");
}

/// A viewer's drag and shortcut never change the deck, not even for a frame.
#[test]
fn read_only_gestures_and_shortcuts_change_nothing() {
    let mut room = Room::new(&deck(false), Access::Read, Access::Edit);
    let alpha = find(&room.ann_doc(), "alpha");
    room.h.state_mut().run("slide.go", json!({"index": 1})).unwrap();
    room.h.state_mut().run("edit.select", json!({"ids": [alpha.0]})).unwrap();
    room.h.run_steps(2);
    let before = room.ann_doc();
    let from = slide_to_screen(&room.h, 160.0, 100.0);
    let to = slide_to_screen(&room.h, 260.0, 180.0);
    let press =
        |pos, pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::default() };
    room.h.input_mut().events.extend([egui::Event::PointerMoved(from), press(from, true)]);
    room.h.run_steps(1);
    room.h.input_mut().events.push(egui::Event::PointerMoved(to));
    room.h.run_steps(1);
    assert!(*before == *room.ann_doc(), "no move while dragging");
    room.h.input_mut().events.extend([press(to, false)]);
    room.h.run_steps(1);
    assert!(*before == *room.ann_doc(), "no move at the drop");
    let cmd = egui::Modifiers { command: true, ctrl: !cfg!(target_os = "macos"), ..Default::default() };
    room.h.input_mut().events.push(egui::Event::Key { key: egui::Key::D, physical_key: None, pressed: true, repeat: false, modifiers: cmd });
    room.h.run_steps(1);
    assert!(*before == *room.ann_doc(), "no duplicate");
}

#[test]
fn a_broken_link_says_so_not_paused() {
    let mut room = Room::editors(&deck(false));
    room.h.state_mut().live.as_mut().unwrap().live_mut().core_mut().end(craft_live::BROKEN);
    room.h.run_steps(1);
    let why = room.h.state().live_refusal("slide.new").map(|s| s.to_string());
    assert_eq!(why.as_deref(), Some(craft_live::BROKEN), "greyed with the link's reason");
    assert_ne!(room.h.state().live_typing_refused().map(|s| s.to_string()).as_deref(), Some(deckcraft_live::PAUSED_TEXT));
}

/// Two shortcut edits in one frame are two groups, as two commands are.
#[test]
fn each_shortcut_edit_is_its_own_group() {
    let mut room = Room::editors(&deck(false));
    let alpha = find(&room.ann_doc(), "alpha");
    room.h.state_mut().run("slide.go", json!({"index": 1})).unwrap();
    room.h.state_mut().run("edit.select", json!({"ids": [alpha.0]})).unwrap();
    room.h.run_steps(1);
    let _ = room.h.state_mut().live.as_mut().unwrap().live_mut().take_sent();
    let cmd = egui::Modifiers { command: true, ctrl: !cfg!(target_os = "macos"), ..Default::default() };
    let key = egui::Event::Key { key: egui::Key::D, physical_key: None, pressed: true, repeat: false, modifiers: cmd };
    room.h.input_mut().events.extend([key.clone(), key]);
    room.h.run_steps(2);
    let sent = room.h.state_mut().live.as_mut().unwrap().live_mut().take_sent();
    assert_eq!(sent.len(), 2, "one group per duplicate");
}

/// Asks the window to close (the close button, Cmd+Q or `app.quit`) for one frame.
fn request_close(h: &mut Harness<'_, SlideApp>) -> bool {
    h.input_mut().viewports.entry(egui::ViewportId::ROOT).or_default().events.push(egui::ViewportEvent::Close);
    h.run_steps(1);
    h.input_mut().viewports.entry(egui::ViewportId::ROOT).or_default().events.clear();
    h.output()
        .viewport_output
        .get(&egui::ViewportId::ROOT)
        .is_some_and(|o| o.commands.iter().any(|c| matches!(c, egui::ViewportCommand::CancelClose)))
}

/// Closing with unsaved changes asks first; Cancel keeps the window in the room (R1).
#[test]
fn a_cancelled_quit_keeps_the_live_session() {
    let mut room = Room::editors(&deck(false));
    let alpha = find(&room.ann_doc(), "alpha");
    let app = room.h.state_mut();
    app.run("slide.go", json!({"index": 1})).unwrap();
    app.run("text.edit", json!({"id": alpha.0})).unwrap();
    app.run("text.insert", json!({"text": "unsaved"})).unwrap();
    room.h.run_steps(1);
    assert!(room.h.state().session.active().unwrap().is_dirty(), "there is something to save");
    assert!(request_close(&mut room.h), "the close waits for the question");
    assert!(room.h.state().dialog.as_ref().is_some_and(|d| d.id == "quit"), "the save question is asked");
    room.h.run_steps(1);
    room.h.get_by_role_and_label(Role::Button, "Cancel").click();
    room.h.run_steps(2);
    assert!(room.h.state().dialog.is_none(), "the question was answered");
    assert!(room.h.state().live.is_some(), "the window is still in the room");
}

/// A close that goes ahead ends the session and offers its copy; a copy not taken holds the close
/// back once, says that live editing ended, and the next close goes ahead.
#[test]
fn closing_the_window_offers_the_copy_once() {
    let mut room = Room::with_services(&deck(false), Access::Edit, Access::Edit, cancelling());
    room.h.state_mut().pending_unmerged.push(copy());
    // The save question was answered (Save or Don't Save), so the close goes ahead.
    room.h.state_mut().quit_confirmed = true;
    room.h.run_steps(1);
    assert!(request_close(&mut room.h), "held back for the copy");
    assert!(room.h.state().live.is_none(), "the window left the room");
    assert_eq!(room.h.state().pending_unmerged, vec![copy()], "the copy still waits");
    let said = room.h.state().toast.as_ref().map(|t| t.text.clone()).unwrap_or_default();
    assert!(said.contains(super::live::LIVE_ENDED_TEXT), "the person is told live editing ended: {said}");
    assert!(!request_close(&mut room.h), "the next close goes ahead");
}

/// On exit every waiting copy is offered: one the person cancels does not stop the next.
#[test]
fn finish_live_offers_every_copy() {
    let log = HostLog::default();
    let asked = HostLog::default();
    let (w, a) = (log.clone(), asked.clone());
    let services = Services {
        // The person cancels the first Save dialog and saves the second copy.
        pick_save: Some(Box::new(move |name: &str| {
            a.0.borrow_mut().push((name.to_owned(), Vec::new()));
            name.contains("1500").then(|| format!("/saved/{name}"))
        })),
        write: Some(Box::new(move |path: &str, b: &[u8]| {
            w.0.borrow_mut().push((path.to_owned(), b.to_vec()));
            Ok(())
        })),
        ..Default::default()
    };
    let mut room = Room::with_services(&deck(false), Access::Edit, Access::Edit, services);
    let second = ("Deck (unmerged 2026-11-23 1500).pptx".to_owned(), deck(true));
    room.h.state_mut().pending_unmerged.extend([copy(), second.clone()]);
    room.h.state_mut().finish_live();
    assert!(room.h.state().live.is_none(), "the session ended");
    let names: Vec<String> = asked.got().into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, vec![copy().0, second.0.clone()], "each copy was offered once");
    assert_eq!(log.got(), vec![(format!("/saved/{}", second.0), second.1)], "the second was written");
    assert_eq!(room.h.state().pending_unmerged, vec![copy()], "the cancelled one still waits");
}

/// Closing the live presentation with file.close hands a waiting copy to the host.
#[test]
fn file_close_hands_a_waiting_copy_over() {
    let log = HostLog::default();
    let mut room = Room::with_services(&deck(false), Access::Edit, Access::Edit, downloading(&log));
    room.h.state_mut().pending_unmerged.push(copy());
    let _ = room.h.state_mut().run("file.close", json!({}));
    room.h.run_steps(2);
    assert!(room.h.state().live.is_none(), "the window left the room");
    assert_eq!(log.got(), vec![copy()], "the waiting copy was handed over");
}

/// A viewer's Replace All is refused with the live reason, not "can't find".
#[test]
fn find_replace_says_the_live_refusal() {
    let mut room = Room::new(&deck(false), Access::Read, Access::Edit);
    room.h.state_mut().dialog = Some(crate::dialogs::Dialog::new("find"));
    room.h.state_mut().dialog.as_mut().unwrap().fields.extend([
        ("text".to_owned(), "alpha".to_owned()),
        ("with".to_owned(), "x".to_owned()),
        ("replace".to_owned(), "1".to_owned()),
    ]);
    room.h.run_steps(2);
    room.h.get_by_role_and_label(Role::Button, "Replace All").click();
    room.h.run_steps(2);
    let status = room.h.state().dialog.as_ref().and_then(|d| d.fields.get("status").cloned()).unwrap_or_default();
    assert_eq!(status, READ_ONLY_TEXT, "the reason, not a missing match");
}

/// Shift+F3 (change case, no Cmd) is refused for a viewer before it runs.
#[test]
fn a_shortcut_without_cmd_is_gated_too() {
    let mut room = Room::new(&deck(false), Access::Read, Access::Edit);
    let alpha = find(&room.ann_doc(), "alpha");
    room.h.state_mut().run("slide.go", json!({"index": 1})).unwrap();
    room.h.state_mut().run("edit.select", json!({"ids": [alpha.0]})).unwrap();
    room.h.run_steps(1);
    let before = room.ann_doc();
    let shift = egui::Modifiers { shift: true, ..Default::default() };
    room.h.input_mut().events.push(egui::Event::Key { key: egui::Key::F3, physical_key: None, pressed: true, repeat: false, modifiers: shift });
    room.h.run_steps(1);
    assert!(*before == *room.ann_doc(), "no change");
    assert_eq!(room.h.state_mut().fresh_status().as_deref(), Some(READ_ONLY_TEXT), "refused up front, with the reason");
}

/// A viewer's single Replace says why it was refused; the next match does not write over it (N1).
#[test]
fn single_replace_says_the_live_refusal() {
    let mut room = Room::new(&deck(false), Access::Read, Access::Edit);
    room.h.state_mut().dialog = Some(crate::dialogs::Dialog::new("find"));
    room.h.state_mut().dialog.as_mut().unwrap().fields.extend([
        ("text".to_owned(), "alpha".to_owned()),
        ("with".to_owned(), "x".to_owned()),
        ("replace".to_owned(), "1".to_owned()),
    ]);
    room.h.run_steps(2);
    room.h.get_by_role_and_label(Role::Button, "Find Next").click();
    room.h.run_steps(2);
    let before = room.ann_doc();
    // The dialog's Replace, beside its Find Next (the ribbon has a Replace too).
    let next = room.h.get_by_role_and_label(Role::Button, "Find Next").rect().center();
    let replace = room
        .h
        .query_all_by_role_and_label(Role::Button, "Replace")
        .min_by(|a, b| a.rect().center().distance(next).total_cmp(&b.rect().center().distance(next)))
        .unwrap();
    replace.click();
    room.h.run_steps(2);
    let status = room.h.state().dialog.as_ref().and_then(|d| d.fields.get("status").cloned()).unwrap_or_default();
    assert_eq!(status, READ_ONLY_TEXT, "the reason stays shown");
    assert!(*before == *room.ann_doc(), "nothing was replaced");
}

/// A desktop write that fails keeps the copy offered, with the error (U1 review F5).
#[test]
fn a_failed_write_keeps_the_copy() {
    let services = Services {
        pick_save: Some(Box::new(|name: &str| Some(format!("/nowhere/{name}")))),
        write: Some(Box::new(|_: &str, _: &[u8]| Err("disk full".to_owned()))),
        ..Default::default()
    };
    let mut room = Room::with_services(&deck(false), Access::Edit, Access::Edit, services);
    room.h.state_mut().pending_unmerged.push(copy());
    room.h.run_steps(2);
    room.h.get_by_role_and_label(Role::Button, super::live::SAVE_COPY_TEXT).click();
    room.h.run_steps(2);
    assert_eq!(room.h.state().pending_unmerged, vec![copy()], "the copy still waits");
    assert_eq!(room.h.state_mut().fresh_status().as_deref(), Some("disk full"), "the error is shown");
}

/// A page that keeps the files gets the copy through `save_copy` (a new file, Ruling D18), not as
/// a download; a refused hand-over keeps it waiting.
#[test]
fn the_copy_goes_to_the_page_as_a_new_file() {
    let downloads = HostLog::default();
    let copies = HostLog::default();
    let mut services = downloading(&downloads);
    let c = copies.clone();
    services.save_copy = Some(Box::new(move |name: &str, b: &[u8]| {
        c.0.borrow_mut().push((name.to_owned(), b.to_vec()));
        Ok(None)
    }));
    let mut room = Room::with_services(&deck(false), Access::Edit, Access::Edit, services);
    room.h.state_mut().pending_unmerged.push(copy());
    room.h.state_mut().hand_over_copies();
    assert_eq!(copies.got(), vec![copy()], "the page got the copy");
    assert!(downloads.got().is_empty(), "not as a download");
    assert!(!room.h.state().copies_unsaved(), "a page that does not answer saves stored it");
}

/// A web host that keeps the files and answers saves.
fn answering(copies: &HostLog, downloads: &HostLog) -> (Services, crate::HostSaves) {
    let saves = crate::HostSaves::default();
    saves.set_confirms();
    let (c, s) = (copies.clone(), saves.clone());
    let d = downloads.clone();
    let services = Services {
        host_saves: saves.clone(),
        save_copy: Some(Box::new(move |name: &str, b: &[u8]| {
            c.0.borrow_mut().push((name.to_owned(), b.to_vec()));
            Ok(s.copy_id())
        })),
        download_copy: Some(Box::new(move |name: &str, b: &[u8]| d.0.borrow_mut().push((name.to_owned(), b.to_vec())))),
        ..Default::default()
    };
    (services, saves)
}

fn sent_id(room: &Room) -> String {
    room.h.state().copies_sent.first().map(|(id, _)| id.clone()).expect("a copy was handed to the page")
}

/// The copy is kept until the page says it stored it; then the person is told the new file's
/// name (Review I1).
#[test]
fn a_copy_the_page_saved_is_named_to_the_person() {
    let (copies, downloads) = (HostLog::default(), HostLog::default());
    let (services, saves) = answering(&copies, &downloads);
    let mut room = Room::with_services(&deck(false), Access::Edit, Access::Edit, services);
    room.h.state_mut().pending_unmerged.push(copy());
    room.h.state_mut().hand_over_copies();
    assert_eq!(copies.got(), vec![copy()], "the page got the copy");
    assert!(room.h.state().copies_unsaved(), "kept until the page answers");
    let id = sent_id(&room);
    saves.answer(crate::HostAnswer::Saved { id, name: Some("Deck (unmerged 2026-11-23 1404) (2).pptx".into()) });
    room.h.run_steps(1);
    assert!(!room.h.state().copies_unsaved(), "nothing waits once stored");
    let said = room.h.state().toast.as_ref().map(|t| t.text.clone()).unwrap_or_default();
    assert_eq!(said, super::live::fill(super::live::COPY_SAVED_TEXT, &[("name", "Deck (unmerged 2026-11-23 1404) (2).pptx")]));
    assert!(downloads.got().is_empty());
}

/// A copy the page failed to store is offered again and goes to a browser download, never back
/// to the page on its own (Review I1, M7).
#[test]
fn a_copy_the_page_failed_to_save_is_offered_as_a_download() {
    let (copies, downloads) = (HostLog::default(), HostLog::default());
    let (services, saves) = answering(&copies, &downloads);
    let mut room = Room::with_services(&deck(false), Access::Edit, Access::Edit, services);
    room.h.state_mut().pending_unmerged.push(copy());
    room.h.state_mut().hand_over_copies();
    let id = sent_id(&room);
    saves.answer(crate::HostAnswer::Failed { id, error: "quota".into() });
    room.h.run_steps(1);
    assert_eq!(room.h.state().pending_unmerged, vec![copy()], "the copy waits again");
    let said = room.h.state().toast.as_ref().map(|t| t.text.clone()).unwrap_or_default();
    assert_eq!(said, super::live::fill(super::live::COPY_FAILED_TEXT, &[("error", "quota")]));
    for _ in 0..3 {
        room.h.state_mut().hand_over_copies();
        room.h.run_steps(1);
    }
    assert_eq!(copies.got().len(), 1, "no automatic second hand-over");
    room.h.get_by_role_and_label(Role::Button, super::live::SAVE_COPY_TEXT).click();
    room.h.run_steps(2);
    assert_eq!(downloads.got(), vec![copy()], "Save a copy downloads it");
    assert!(!room.h.state().copies_unsaved());
}

/// A hand-over the page could not take keeps the copy and is not retried every frame.
#[test]
fn a_hand_over_that_could_not_be_posted_is_not_retried() {
    let (downloads, tries) = (HostLog::default(), HostLog::default());
    let t = tries.clone();
    let d = downloads.clone();
    let services = Services {
        save_copy: Some(Box::new(move |name: &str, b: &[u8]| {
            t.0.borrow_mut().push((name.to_owned(), b.to_vec()));
            Err("the page is gone".to_owned())
        })),
        download_copy: Some(Box::new(move |name: &str, b: &[u8]| d.0.borrow_mut().push((name.to_owned(), b.to_vec())))),
        ..Default::default()
    };
    let mut room = Room::with_services(&deck(false), Access::Edit, Access::Edit, services);
    room.h.state_mut().pending_unmerged.push(copy());
    for _ in 0..3 {
        room.h.state_mut().hand_over_copies();
    }
    assert_eq!(tries.got().len(), 1, "tried once");
    assert!(room.h.state().copies_waiting(), "a refused hand-over keeps the copy");
    room.h.state_mut().offer_unmerged();
    assert_eq!(downloads.got(), vec![copy()], "Save a copy downloads it");
}

/// With the live presentation closed and no other open, a waiting copy is still offered on the
/// start screen (U1 review F2).
#[test]
fn a_waiting_copy_is_offered_on_the_start_screen() {
    let mut room = Room::with_services(&deck(false), Access::Edit, Access::Edit, cancelling());
    room.h.state_mut().pending_unmerged.push(copy());
    let _ = room.h.state_mut().run("file.close", json!({}));
    room.h.run_steps(3);
    assert!(room.h.state().session.active().is_none(), "no presentation is open");
    room.h.get_by_role_and_label(Role::Button, super::live::SAVE_COPY_TEXT);
}

/// A host page starts with an empty presentation; the live one takes its place and is shown.
#[test]
fn a_live_start_takes_the_place_of_the_hosts_empty_presentation() {
    let bytes = deck(false);
    let relay = MemRelay::new();
    let mut app = SlideApp::new(Session::new(), Services::default());
    app.start_for_host().unwrap();
    let base = BaseFile { bytes: &bytes, name: NAME, expected_sha256: None };
    let live = DeckLive::start(BoxHub(Box::new(relay.connect("ann", "Ann", true))), ROOM, &base, Access::Edit, "ann", &mut app.session).unwrap();
    let doc = live.doc();
    app.start_live(live);
    assert_eq!(app.session.documents().len(), 1, "the empty presentation gave way");
    assert_eq!(app.session.active().map(|d| d.uid), Some(doc), "the live presentation is shown");
}

// --- Share for the page's live presentation (SH2) --------------------------------------------

/// A window framed by a page that shares files (`share: true`), started for the host as the web
/// shell does, on a live session started on `hub` with `access`; and the count of share requests.
fn page_window(hub: BoxHub, access: Access, uid: &str) -> (SlideApp, Arc<AtomicU64>) {
    let bytes = deck(false);
    let asked = Arc::new(AtomicU64::new(0));
    let counter = asked.clone();
    let services = Services {
        host_share: Some(Box::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })),
        ..Default::default()
    };
    services.host_shares.set();
    let mut app = SlideApp::new(Session::new(), services);
    app.start_for_host().unwrap();
    let base = BaseFile { bytes: &bytes, name: NAME, expected_sha256: None };
    let live = DeckLive::start(hub, ROOM, &base, access, uid, &mut app.session).unwrap();
    app.start_live(live);
    (app, asked)
}

/// Share, run now, asks the page once more and shows no "No Nextcloud connected".
fn share_asks_the_page(app: &mut SlideApp, asked: &AtomicU64, case: &str) {
    let before = asked.load(Ordering::Relaxed);
    app.toast = None;
    let r = app.run("file.share", json!({})).unwrap();
    assert_eq!(asked.load(Ordering::Relaxed), before + 1, "{case}: {r}: the page is asked to share the live presentation it joined");
    assert!(app.toast.is_none(), "{case}: no \"No Nextcloud connected\" for the page's own live file");
}

fn frames(app: SlideApp) -> Harness<'static, SlideApp> {
    Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_ui_state(
        |ui, app: &mut SlideApp| {
            app.logic(ui.ctx());
            app.ui(ui);
        },
        app,
    )
}

/// On the web the page's file opens live (the live start opens the room's presentation, not the
/// page's own open), and Share must still ask the page for it.
#[test]
fn share_asks_the_page_for_the_live_presentation_it_joined() {
    let relay = MemRelay::new();
    let (mut app, asked) = page_window(BoxHub(Box::new(relay.connect("ann", "Ann", true))), Access::Edit, "ann");
    share_asks_the_page(&mut app, &asked, "an editor");
}

/// A read-only or guest ticket: Share is not an edit, so the live gate does not refuse it; whether
/// this person may share is the Nextcloud page's call.
#[test]
fn share_asks_the_page_for_a_viewer_of_the_live_presentation() {
    let relay = MemRelay::new();
    let bytes = deck(false);
    let base = BaseFile { bytes: &bytes, name: NAME, expected_sha256: None };
    let mut bob_session = Session::new();
    let _bob = DeckLive::start(relay.connect("bob", "Bob", true), ROOM, &base, Access::Edit, "bob", &mut bob_session).unwrap();
    let (mut app, asked) = page_window(BoxHub(Box::new(relay.connect("vic", "Vic", false))), Access::Read, "vic");
    share_asks_the_page(&mut app, &asked, "a read-only viewer");
}

/// The live session ended: the presentation is still the page's file, so Share still asks the page.
#[test]
fn share_asks_the_page_after_the_live_session_ended() {
    let relay = MemRelay::new();
    let (app, asked) = page_window(BoxHub(Box::new(relay.connect("ann", "Ann", true))), Access::Edit, "ann");
    let mut h = frames(app);
    h.run_steps(3);
    h.state_mut().end_live();
    h.run_steps(3);
    assert!(h.state().live.is_none(), "the window left the live session");
    share_asks_the_page(h.state_mut(), &asked, "after the live session ended");
}

/// A renewed ticket switches the link under the same session: Share keeps asking the page.
#[test]
fn share_asks_the_page_after_a_renewed_ticket() {
    let relay = MemRelay::new();
    let r2 = relay.clone();
    let hub = super::live::renewing(relay.connect("ann", "Ann", true), move |_: &craft_collab::Reopen<'_>| Ok(r2.connect("ann", "Ann", true)));
    let (app, asked) = page_window(hub, Access::Edit, "ann");
    let mut h = frames(app);
    h.run_steps(3);
    use craft_collab::Hub as _;
    h.state_mut().live.as_mut().unwrap().live_mut().core_mut().hub_mut().set_token("renewed".into());
    h.run_steps(10);
    assert!(h.state().live.is_some(), "still live after the renewal");
    share_asks_the_page(h.state_mut(), &asked, "after a renewed ticket");
}

/// A viewer's armed Format Painter does not apply on a click (F7): refused up front, with why.
#[test]
fn a_viewers_format_painter_is_refused() {
    let mut room = Room::new(&deck(false), Access::Read, Access::Edit);
    let alpha = find(&room.ann_doc(), "alpha");
    let beta = find(&room.ann_doc(), "beta");
    room.h.state_mut().run("slide.go", json!({"index": 1})).unwrap();
    room.h.state_mut().run("edit.select", json!({"ids": [alpha.0]})).unwrap();
    room.h.state_mut().session.execute("format.painter", &json!({})).unwrap();
    room.h.run_steps(2);
    let before = room.ann_doc();
    let at = slide_to_screen(&room.h, 420.0, 240.0);
    let press =
        |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::default() };
    room.h.input_mut().events.extend([egui::Event::PointerMoved(at), press(true)]);
    room.h.run_steps(1);
    room.h.input_mut().events.push(press(false));
    room.h.run_steps(2);
    assert!(*before == *room.ann_doc(), "{beta:?} was not restyled, not even for a frame");
    assert_eq!(room.h.state_mut().fresh_status().as_deref(), Some(READ_ONLY_TEXT), "with the reason");
}

/// A viewer can still marquee-select: it changes nothing (F7).
#[test]
fn a_viewer_can_marquee_select() {
    let mut room = Room::new(&deck(false), Access::Read, Access::Edit);
    let alpha = find(&room.ann_doc(), "alpha");
    room.h.state_mut().run("slide.go", json!({"index": 1})).unwrap();
    room.h.run_steps(2);
    let before = room.ann_doc();
    let from = slide_to_screen(&room.h, 20.0, 20.0);
    let to = slide_to_screen(&room.h, 290.0, 170.0);
    let press =
        |pos, pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::default() };
    room.h.input_mut().events.extend([egui::Event::PointerMoved(from), press(from, true)]);
    room.h.run_steps(1);
    room.h.input_mut().events.push(egui::Event::PointerMoved(from + (to - from) * 0.5));
    room.h.run_steps(1);
    room.h.input_mut().events.push(egui::Event::PointerMoved(to));
    room.h.run_steps(1);
    room.h.input_mut().events.push(press(to, false));
    room.h.run_steps(2);
    assert_eq!(room.ann().session.active().unwrap().selection.shapes, vec![alpha], "the marquee selected");
    assert!(*before == *room.ann_doc(), "and changed nothing");
}

/// In a deck with sections, the Delete key on selected slides and pasting slides are refused with
/// why, as the slide commands are (U1 review M7).
#[test]
fn slide_delete_key_and_slide_paste_are_gated_with_sections() {
    let mut room = Room::editors(&deck(true));
    room.h.run_steps(1);
    let second = room.ann_doc().slides[1].id;
    {
        let st = room.h.state_mut().session.active_mut().unwrap();
        st.selection.shapes.clear();
        st.selection.text = None;
        st.selection.slides = vec![second];
    }
    assert_eq!(room.h.state().live_refusal("edit.delete"), Some(SECTIONS_TEXT), "Delete on slides is greyed with why");
    let before = room.ann_doc();
    room.h.input_mut().events.push(egui::Event::Key {
        key: egui::Key::Delete,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Default::default(),
    });
    room.h.run_steps(2);
    assert!(*before == *room.ann_doc(), "the Delete key removed no slide");
    assert_eq!(room.h.state_mut().fresh_status().as_deref(), Some(SECTIONS_TEXT), "and said why");
    room.h.state_mut().run("edit.copy", json!({})).unwrap();
    assert!(!crate::ribbon::enabled(room.ann(), "edit.paste"), "pasting slides is greyed");
    assert_eq!(crate::ribbon::gate(room.ann(), "edit.paste").why, Some(SECTIONS_TEXT), "with the reason");
}

fn press(room: &mut Room, key: egui::Key) {
    room.h.input_mut().events.push(egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() });
    room.h.run_steps(2);
}

/// Enter on the slide with nothing selected adds a slide in the engine, with no shortcut of its
/// own: in a deck with sections it is refused up front, as the Delete key is (Review M2).
#[test]
fn enter_with_nothing_selected_adds_no_slide_with_sections() {
    let mut room = Room::editors(&deck(true));
    room.h.run_steps(1);
    {
        let st = room.h.state_mut().session.active_mut().unwrap();
        st.selection.shapes.clear();
        st.selection.text = None;
    }
    let slides = room.ann_doc().slides.len();
    room.h.input_mut().events.push(egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Default::default(),
    });
    room.h.step();
    assert_eq!(room.ann_doc().slides.len(), slides, "Enter added no slide, not even for a frame");
    room.h.run_steps(1);
    assert_eq!(room.h.state_mut().fresh_status().as_deref(), Some(SECTIONS_TEXT), "and said why");
}

/// Enter finishes a freeform being drawn, also in a live deck with sections: the engine finishes
/// the path before it would add a slide (U2 fix-1 review N2).
#[test]
fn enter_finishes_a_freeform_live_with_sections() {
    let mut room = Room::editors(&deck(true));
    room.h.run_steps(1);
    let shapes = room.ann_doc().slides[0].shapes.len();
    let ev = |kind, x, y| PointerEvent { kind, x, y, mods: Mods::default(), tol: 4.0, pressure: None };
    {
        let s = &mut room.h.state_mut().session;
        s.set_tool(deckcraft_engine::tools::ToolKind::Shape { preset: "curve".into() });
        for (x, y) in [(300.0, 300.0), (400.0, 320.0), (420.0, 400.0)] {
            s.pointer(ev(PointerKind::Down, x, y)).unwrap();
            s.pointer(ev(PointerKind::Up, x, y)).unwrap();
        }
        assert!(s.tool.drawing_freeform(), "drawing");
    }
    press(&mut room, egui::Key::Enter);
    assert!(!room.h.state().session.tool.drawing_freeform(), "Enter finished the path");
    assert_ne!(room.h.state_mut().fresh_status().as_deref(), Some(SECTIONS_TEXT), "and was not refused");
    room.settle();
    assert_eq!(room.ann_doc().slides[0].shapes.len(), shapes + 1, "the path is on the slide");
    assert_eq!(room.bob.doc().slides[0].shapes.len(), shapes + 1, "and reached Bob");
}

/// The keys an editor types with still work live in a deck with sections: Backspace and Tab in
/// text, Delete on a shape (U2 concern 7).
#[test]
fn editing_keys_are_not_refused_live_with_sections() {
    let mut room = Room::editors(&deck(true));
    let (alpha, beta) = (find(&room.ann_doc(), "alpha"), find(&room.ann_doc(), "beta"));
    room.h.state_mut().run("slide.go", json!({"index": 1})).unwrap();
    room.h.state_mut().run("text.edit", json!({"id": alpha.0})).unwrap();
    room.h.run_steps(1);
    press(&mut room, egui::Key::End);
    press(&mut room, egui::Key::Backspace);
    press(&mut room, egui::Key::Tab);
    room.settle();
    let text = |p: &Presentation| p.slides[1].shape(alpha).unwrap().text.as_ref().unwrap().text();
    // AutoCorrect capitalises the first letter of the sentence as the text is edited.
    assert_eq!(text(&room.ann_doc()).to_lowercase(), "alph\t", "Backspace and Tab typed");
    assert_eq!(text(&room.bob.doc()), text(&room.ann_doc()), "and reached Bob");
    room.h.state_mut().run("text.exit", json!({})).unwrap();
    room.h.state_mut().run("edit.select", json!({"ids": [beta.0]})).unwrap();
    room.h.run_steps(1);
    press(&mut room, egui::Key::Delete);
    room.settle();
    assert!(room.ann_doc().slides[1].shape(beta).is_none(), "Delete removed the shape");
    assert!(room.bob.doc().slides[1].shape(beta).is_none(), "for Bob too");
    assert_eq!(room.ann_doc().slides.len(), 3, "and no slide");
}

/// Clicking the badge of an editor whose place is not known (their slide was deleted, or they
/// selected nothing yet) says so instead of doing nothing (L7 note).
#[test]
fn following_an_editor_without_a_place_says_so() {
    let mut room = Room::editors(&deck(false));
    room.bob.run("slide.go", json!({"index": 2}));
    room.settle();
    // Ann deletes the slide Bob is on: his caret is dropped here for good, until he selects again
    // (Bob does not pump here, so he has not moved yet).
    room.h.state_mut().run("slide.go", json!({"index": 2})).unwrap();
    room.h.state_mut().run("slide.delete", json!({})).unwrap();
    room.h.run_steps(2);
    room.h.get_by_role_and_label(Role::Button, "Bob").click();
    room.h.run_steps(2);
    let said = room.h.state_mut().fresh_status().unwrap_or_default();
    assert_eq!(said, super::live::fill(super::live::NO_PLACE_TEXT, &[("name", "Bob")]), "the person is told");
}

/// DP1: Ctrl+X on a selected shape in a browser (the key and the DOM `cut` in one batch) cuts the
/// shape, not its slide; Ctrl+V pastes it back once; Bob's page converges; one Undo takes the
/// paste back on both pages.
#[test]
fn web_cut_and_paste_of_a_shape_converges_and_undoes_in_one_step() {
    let mut room = Room::editors(&deck(false));
    let alpha = find(&room.ann_doc(), "alpha");
    room.h.state_mut().run("slide.go", json!({"index": 1})).unwrap();
    room.h.state_mut().run("edit.select", json!({"ids": [alpha.0]})).unwrap();
    room.h.run_steps(1);
    let slides = room.ann_doc().slides.len();
    let shapes = room.ann_doc().slides[1].shapes.len();
    let clip = super::clipboard_tests::web_shortcut(&mut room.h, egui::Key::X, Some(egui::Event::Cut)).expect("the cut reached the clipboard");
    room.settle();
    for (who, d) in [("Ann", room.ann_doc()), ("Bob", room.bob.doc())] {
        assert_eq!(d.slides.len(), slides, "{who}: no slide cut");
        assert!(d.slides[1].shape(alpha).is_none(), "{who}: alpha is cut");
    }
    super::clipboard_tests::web_shortcut(&mut room.h, egui::Key::V, Some(egui::Event::Paste(clip)));
    room.settle();
    let texts = |d: &Presentation| d.slides[1].shapes.iter().filter(|s| s.text.as_ref().is_some_and(|t| t.text() == "alpha")).count();
    for (who, d) in [("Ann", room.ann_doc()), ("Bob", room.bob.doc())] {
        assert_eq!(d.slides[1].shapes.len(), shapes, "{who}: the shape is back once");
        assert_eq!(texts(&d), 1, "{who}: one alpha");
    }
    assert_eq!(*room.ann_doc().slides[1], *room.bob.doc().slides[1], "the pages agree");
    room.h.state_mut().run("edit.undo", json!({})).unwrap();
    room.settle();
    for (who, d) in [("Ann", room.ann_doc()), ("Bob", room.bob.doc())] {
        assert_eq!(texts(&d), 0, "{who}: one Undo takes the paste back");
    }
}

/// DP1: Ann's caret at the end of "alpha" (End pressed) stays at the end when Bob types at the
/// start; what Ann types next goes after her text, on both pages.
#[test]
fn my_caret_follows_a_remote_insert_before_it() {
    let mut room = Room::editors(&deck(false));
    let alpha = find(&room.ann_doc(), "alpha");
    room.h.state_mut().run("slide.go", json!({"index": 1})).unwrap();
    room.h.state_mut().run("text.edit", json!({"id": alpha.0, "at": [0, 1]})).unwrap();
    room.h.run_steps(1);
    let end = egui::Event::Key { key: egui::Key::End, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
    room.h.input_mut().events.push(end);
    room.h.run_steps(1);
    room.bob.run("slide.go", json!({"index": 1}));
    room.bob.run("text.edit", json!({"id": alpha.0, "at": [0, 0]}));
    room.bob.run("text.insert", json!({"text": "Bob "}));
    room.settle();
    room.h.input_mut().events.push(egui::Event::Text("!".into()));
    room.h.run_steps(1);
    room.settle();
    for (who, d) in [("Ann", room.ann_doc()), ("Bob", room.bob.doc())] {
        let t = d.slides[1].shape(alpha).unwrap().text.as_ref().unwrap().text();
        assert_eq!(t, "Bob alpha!", "{who}: Ann's text is at the end");
    }
}

/// The text Ann has selected.
fn selected_text(room: &Room) -> String {
    let st = room.ann().session.active().unwrap();
    let sel = st.selection.text.clone().unwrap();
    let ((_, a), (_, b)) = sel.ordered();
    let t = st.doc.slides[st.selection.slide].shape(sel.shape).unwrap().text.as_ref().unwrap().text();
    t.chars().skip(a).take(b - a).collect()
}

/// DP1 (review m3): Ann has "alpha" selected; Bob types before it. Both ends of Ann's selection
/// move with the text, so it still covers "alpha", and what Ann types replaces "alpha" on both
/// pages.
#[test]
fn my_text_selection_follows_a_remote_insert_before_it() {
    let mut room = Room::editors(&deck(false));
    let alpha = find(&room.ann_doc(), "alpha");
    room.h.state_mut().run("slide.go", json!({"index": 1})).unwrap();
    room.h.state_mut().run("text.edit", json!({"id": alpha.0, "at": [0, 1]})).unwrap();
    room.h.state_mut().run("text.selectAll", json!({})).unwrap();
    room.h.run_steps(1);
    assert_eq!(selected_text(&room), "alpha");
    let before = room.ann().session.active().unwrap().selection.text.clone().unwrap();
    room.bob.run("slide.go", json!({"index": 1}));
    room.bob.run("text.edit", json!({"id": alpha.0, "at": [0, 0]}));
    room.bob.run("text.insert", json!({"text": "Bob "}));
    room.settle();
    let after = room.ann().session.active().unwrap().selection.text.clone().unwrap();
    assert_eq!(after.anchor, (before.anchor.0, before.anchor.1 + 4), "the anchor moved");
    assert_eq!(after.caret, (before.caret.0, before.caret.1 + 4), "the caret moved");
    assert_eq!(selected_text(&room), "alpha", "the selection still covers alpha");
    room.h.input_mut().events.push(egui::Event::Text("X".into()));
    room.h.run_steps(1);
    room.settle();
    for (who, d) in [("Ann", room.ann_doc()), ("Bob", room.bob.doc())] {
        let t = d.slides[1].shape(alpha).unwrap().text.as_ref().unwrap().text();
        assert_eq!(t, "Bob X", "{who}: Ann's typing replaced her selection");
    }
}

/// DP1, as the browser test does it: a double-click into the text, a remote insert at its start
/// while the double-click is still settling, then End and typing.
#[test]
fn my_caret_follows_a_remote_insert_after_a_double_click() {
    for remote_before_end in [false, true] {
        let mut room = Room::editors(&deck(false));
        let alpha = find(&room.ann_doc(), "alpha");
        room.h.state_mut().run("slide.go", json!({"index": 1})).unwrap();
        room.bob.run("slide.go", json!({"index": 1}));
        room.h.run_steps(2);
        let at = slide_to_screen(&room.h, 160.0, 100.0);
        let press =
            |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::default() };
        room.h.input_mut().events.push(egui::Event::PointerMoved(at));
        room.h.run_steps(1);
        for _ in 0..2 {
            room.h.input_mut().events.push(press(true));
            room.h.run_steps(1);
            room.h.input_mut().events.push(press(false));
            room.h.run_steps(1);
        }
        assert!(room.ann().session.active().unwrap().selection.text.is_some(), "the double-click edits the text");
        let end = egui::Event::Key { key: egui::Key::End, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
        let bob_types = |room: &mut Room| {
            room.bob.run("text.edit", json!({"id": alpha.0, "at": [0, 0]}));
            room.bob.run("text.insert", json!({"text": "Bob "}));
            room.settle();
        };
        if remote_before_end {
            // Bob's "Bob " before Ann's caret (the harness's double-click leaves it collapsed, a
            // range is `my_text_selection_follows_a_remote_insert_before_it`) moves both ends of
            // her selection by 4 (review m3).
            let before = room.ann().session.active().unwrap().selection.text.clone().unwrap();
            bob_types(&mut room);
            let after = room.ann().session.active().unwrap().selection.text.clone().unwrap();
            assert_eq!((after.anchor, after.caret), ((before.anchor.0, before.anchor.1 + 4), (before.caret.0, before.caret.1 + 4)));
            room.h.input_mut().events.push(end);
            room.h.run_steps(1);
        } else {
            room.h.input_mut().events.push(end);
            room.h.run_steps(1);
            bob_types(&mut room);
        }
        for c in ["!", "?"] {
            room.h.input_mut().events.push(egui::Event::Text(c.into()));
            room.h.run_steps(1);
        }
        room.settle();
        for (who, d) in [("Ann", room.ann_doc()), ("Bob", room.bob.doc())] {
            let t = d.slides[1].shape(alpha).unwrap().text.as_ref().unwrap().text();
            assert_eq!(t, "Bob alpha!?", "{who}, remote before End {remote_before_end}");
        }
    }
}

/// DP1 fix 1 (review I2): a slide with an animation is not copied live (Ruling D15). A browser's
/// Ctrl+C, Ctrl+V on it pastes nothing on either page and says why, as the menu's Paste does.
#[test]
fn web_paste_of_an_animated_slide_is_refused_with_the_reason() {
    let mut s = Session::with_new();
    s.execute("slide.new", &json!({"layout": "blank"})).unwrap();
    let id = s.execute("shape.insert", &json!({"preset": "rect", "rect": [60.0, 60.0, 200.0, 80.0], "text": "alpha"})).unwrap()["id"].clone();
    s.execute("animation.add", &json!({"effect": "fade", "class": "entrance", "start": "onClick", "ids": [id]})).unwrap();
    s.execute("slide.go", &json!({"index": 0})).unwrap();
    let bytes = deckcraft_engine::cmd::file::save_bytes(&s.active().unwrap().doc, "pptx").unwrap();
    let mut room = Room::editors(&bytes);
    room.h.state_mut().run("slide.go", json!({"index": 1})).unwrap();
    room.h.state_mut().run("slide.selectRange", json!({"to": 1, "from": 1})).unwrap();
    room.h.run_steps(1);
    let n = room.ann_doc().slides.len();
    let clip = crate::clipboard_tests::web_shortcut(&mut room.h, egui::Key::C, Some(egui::Event::Copy)).unwrap_or_default();
    crate::clipboard_tests::web_shortcut(&mut room.h, egui::Key::V, Some(egui::Event::Paste(clip)));
    room.settle();
    assert_eq!(room.ann_doc().slides.len(), n, "Ann: nothing pasted");
    assert_eq!(room.bob.doc().slides.len(), n, "Bob: nothing pasted");
    let status = room.ann().status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
    assert!(status.contains("can't be copied while editing live"), "Ann is told why: {status:?}");
}

/// A test link whose relay messages, `Joined` among them, are held back for `hold` polls: a
/// renewed link that is still joining while the person types (DC1 review m3).
struct Slow {
    hub: MemHub,
    hold: usize,
    held: Vec<craft_collab::ServerMsg>,
}

impl Slow {
    fn new(hub: MemHub, hold: usize) -> Slow {
        Slow { hub, hold, held: Vec::new() }
    }
}

impl craft_collab::Hub for Slow {
    fn send(&mut self, msg: craft_collab::ClientMsg) -> craft_collab::Result<()> {
        self.hub.send(msg)
    }
    fn poll(&mut self) -> Vec<craft_collab::ServerMsg> {
        if self.hold > 0 {
            self.hold -= 1;
            let got = self.hub.poll();
            self.held.extend(got);
            return Vec::new();
        }
        let mut out = std::mem::take(&mut self.held);
        out.extend(self.hub.poll());
        out
    }
    fn connected(&self) -> bool {
        self.hub.connected()
    }
    fn set_token(&mut self, token: String) {
        self.hub.set_token(token);
    }
    fn set_space(&mut self, space: u32) {
        self.hub.set_space(space);
    }
    fn close(&mut self) {
        self.hub.close();
    }
}

/// DC1 review m2: closing a live presentation whose hub renews tickets closes the link in use
/// (`BoxHub` forwards `Hub::close` to the `Handover`). The relay link has no `Drop`, so without
/// the forward it stays in the room.
#[test]
fn closing_a_renewing_live_presentation_leaves_the_room() {
    let bytes = deck(false);
    let relay = MemRelay::new();
    let first = relay.connect("ann", "Ann", true);
    let r2 = relay.clone();
    let hub = super::live::renewing(first.clone(), move |_: &craft_collab::Reopen<'_>| Ok(r2.connect("ann", "Ann", true)));
    let mut session = Session::new();
    let base = BaseFile { bytes: &bytes, name: NAME, expected_sha256: None };
    let live = DeckLive::start(hub, ROOM, &base, Access::Edit, "ann", &mut session).unwrap();
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_ui_state(
        |ui, app: &mut SlideApp| {
            app.logic(ui.ctx());
            app.ui(ui);
        },
        SlideApp::new(session, Services { live: Some(live), ..Default::default() }),
    );
    h.run_steps(3);
    use craft_collab::Hub as _;
    assert!(first.connected(), "in the room while open");
    let _ = h.state_mut().run("file.close", json!({}));
    h.run_steps(2);
    assert!(h.state().live.is_none(), "the window let the live session go");
    assert!(!first.connected(), "and closed its link to the room");
}

/// DC1 (LP2's switch on DeckCraft): Ann's hub is the web host's, [`super::live::renewing`]. A
/// renewed ticket opens a second link; once it has joined Ann is on it, and the old ticket's
/// expiry (its link dropped by the relay) costs nothing: what Ann typed before, during and after
/// the switch reaches Bob in order, Ann stays Live the whole time, and Bob sees one Ann.
#[test]
fn a_renewed_ticket_switches_links_with_no_drop_and_no_lost_typing() {
    let bytes = deck(false);
    let relay = MemRelay::new();
    let clock = Arc::new(AtomicU64::new(0));
    let base = BaseFile { bytes: &bytes, name: NAME, expected_sha256: None };
    let first = relay.connect("ann", "Ann", true);
    let opened = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let (r2, o2) = (relay.clone(), opened.clone());
    // The renewed link joins only after 3 polls, so typing goes on while it is pending.
    let hub = super::live::renewing(Slow::new(first.clone(), 0), move |r: &craft_collab::Reopen<'_>| {
        o2.lock().unwrap().push(r.ticket.to_owned());
        Ok(Slow::new(r2.connect("ann", "Ann", true), 3))
    });
    let mut session = Session::new();
    let mut live = DeckLive::start(hub, ROOM, &base, Access::Edit, "ann", &mut session).unwrap();
    let c = clock.clone();
    live.set_clock(Box::new(move || c.load(Ordering::Relaxed) as f64));
    let app = SlideApp::new(session, Services { live: Some(live), ..Default::default() });
    let h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_ui_state(
        |ui, app: &mut SlideApp| {
            app.logic(ui.ctx());
            app.ui(ui);
        },
        app,
    );
    let hub = relay.connect("bob", "Bob", true);
    let mut session = Session::new();
    let mut live = DeckLive::start(hub, ROOM, &base, Access::Edit, "bob", &mut session).unwrap();
    let c = clock.clone();
    live.set_clock(Box::new(move || c.load(Ordering::Relaxed) as f64));
    let mut room = Room { relay: relay.clone(), ann_hub: first.clone(), h, bob: Bob { live, session }, clock };
    room.settle();

    let alpha = find(&room.ann_doc(), "alpha");
    room.h.state_mut().run("slide.go", json!({"index": 1})).unwrap();
    room.h.state_mut().run("text.edit", json!({"id": alpha.0, "at": [0, 5]})).unwrap();
    room.h.run_steps(1);
    let live_now = |room: &Room| matches!(room.ann().live.as_ref().unwrap().live().status(), craft_live::LiveStatus::Live { .. });
    let typed = |room: &mut Room, t: &str| {
        room.h.input_mut().events.push(egui::Event::Text(t.into()));
        room.h.run_steps(1);
    };
    typed(&mut room, "1");
    room.settle();
    // The page's ticket refresh, as the host hands it in.
    use craft_collab::Hub as _;
    room.h.state_mut().live.as_mut().unwrap().live_mut().core_mut().hub_mut().set_token("renewed".into());
    for _ in 0..3 {
        typed(&mut room, "2");
        assert!(live_now(&room), "Live while the new link joins");
    }
    room.settle();
    assert_eq!(*opened.lock().unwrap(), vec!["renewed".to_owned()], "one new link, opened with the renewed ticket");
    // The switch closed the old link (review m1): it is not in the room, and Bob sees one Ann.
    assert!(!first.connected(), "the old link was closed at the switch");
    let peers = room.bob.live.pump(&mut room.bob.session);
    assert_eq!(peers.iter().filter(|p| craft_live::uid_of(&p.id) == "ann").count(), 1, "Bob sees one Ann after the switch: {peers:?}");
    typed(&mut room, "3");
    // The old ticket expires: the relay drops the first link.
    room.relay.drop_link(&first);
    for _ in 0..3 {
        typed(&mut room, "4");
        assert!(live_now(&room), "Live across the old ticket's expiry");
        room.settle();
    }
    let at_bob = room.bob.doc().slides[1].shape(alpha).unwrap().text.as_ref().unwrap().text();
    assert_eq!(at_bob, "alpha12223444", "every keystroke reached Bob, in order");
    let peers = room.bob.live.pump(&mut room.bob.session);
    assert_eq!(peers.iter().filter(|p| craft_live::uid_of(&p.id) == "ann").count(), 1, "Bob sees one Ann: {peers:?}");
    let ann = room.ann_doc().slides[1].shape(alpha).unwrap().text.as_ref().unwrap().text();
    assert_eq!(ann, at_bob, "both pages agree");
    // Closing after the switch closes the renewed link too (review m2).
    let _ = room.h.state_mut().run("file.close", json!({}));
    room.settle();
    let peers = room.bob.live.pump(&mut room.bob.session);
    assert!(peers.iter().all(|p| craft_live::uid_of(&p.id) != "ann"), "Ann left the room: {peers:?}");
}

/// DP1 parked item (DC1): Ctrl+Shift+V's paste is shared at once, as its key was before DP1
/// moved it onto the browser's paste event (`chord_paste` flushes like the key path). In one
/// batch with typing after it, the paste goes out as its own update ahead of the typing: the
/// relay orders two ops, where one unflushed frame sends one.
#[test]
fn a_web_ctrl_shift_v_paste_is_shared_before_the_typing_after_it() {
    let mut room = Room::editors(&deck(false));
    let alpha = find(&room.ann_doc(), "alpha");
    room.h.state_mut().run("slide.go", json!({"index": 1})).unwrap();
    room.h.state_mut().run("text.edit", json!({"id": alpha.0, "at": [0, 5]})).unwrap();
    room.settle();
    let chord =
        egui::Modifiers { command: true, ctrl: !cfg!(target_os = "macos"), mac_cmd: cfg!(target_os = "macos"), shift: true, ..Default::default() };
    let key = |pressed| egui::Event::Key { key: egui::Key::V, physical_key: None, pressed, repeat: false, modifiers: chord };
    let before = room.relay.seq();
    room.h.input_mut().events.extend([key(true), egui::Event::Paste("P".into()), key(false), egui::Event::Text("t".into())]);
    room.h.run_steps(1);
    room.settle();
    let t = room.bob.doc().slides[1].shape(alpha).unwrap().text.as_ref().unwrap().text();
    assert_eq!(t, "alphaPt", "the paste and the typing reached Bob once each");
    assert_eq!(room.relay.seq() - before, 2, "the paste was shared on its own, before the typing");
}
