//! The live interface in the window: a real room on craft-live's in-memory relay, with this
//! window (Ann) in a kittest harness and a second editor (Bob) as a bare engine session.
//! Test decks are built in code through registered commands and saved as `.pptx` (rule 24).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use craft_live::Access;
use craft_live::testing::{MemHub, MemRelay, ROOM};
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

#[test]
fn undo_while_live_says_not_yet() {
    let mut room = Room::editors(&deck(false));
    room.h.run_steps(1);
    assert!(crate::ribbon::enabled(room.ann(), "edit.undo"), "Undo is offered while live");
    let _ = room.h.state_mut().run("edit.undo", json!({}));
    room.h.run_steps(2);
    room.h.get_by_label_contains(NOT_YET_TEXT);
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
