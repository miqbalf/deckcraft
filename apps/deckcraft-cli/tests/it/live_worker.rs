//! `live-worker`'s document service against an editor in an in-memory room: the file the worker
//! saves is the presentation the editors see. Decks are built in code through registered
//! commands (rule 24).

// The service is part of the binary; the tests compile the same file.
#[path = "../../src/live_worker.rs"]
mod service;

use std::sync::Arc;

use craft_live::testing::{MemHub, MemRelay, ROOM};
use craft_live::{Access, SERVICE_UID};
use craft_live_worker::{DocumentService, LiveDocument};
use deckcraft_engine::Session;
use deckcraft_engine::cmd::file::{add_opened, open_presentation, save_bytes};
use deckcraft_live::{BaseFile, DeckLive};
use deckcraft_model::{LIVE_ID_BASE, Presentation, Shape, ShapeId, ShapeKind};
use serde_json::{Value, json};
use service::{DeckService, DeckWorkerDoc};

const NAME: &str = "Deck.pptx";

/// 8×8 red PNG.
const PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 8, 0, 0, 0, 8, 8, 2, 0, 0, 0, 75, 109, 41, 220, 0, 0, 0, 17, 73, 68, 65,
    84, 120, 156, 99, 184, 163, 161, 129, 21, 49, 12, 45, 9, 0, 123, 123, 75, 1, 251, 162, 81, 240, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

fn exec(s: &mut Session, id: &str, params: Value) -> Value {
    s.execute(id, &params).unwrap_or_else(|e| panic!("{id}: {e}"))
}

fn id_of(v: &Value) -> ShapeId {
    ShapeId(v.get("id").and_then(Value::as_u64).expect("an id") as u32)
}

/// A box with `text`, named `text`, at `rect`.
fn add_box(s: &mut Session, text: &str, rect: [f64; 4]) -> ShapeId {
    let id = id_of(&exec(s, "shape.insert", json!({"preset": "rect", "rect": rect, "text": text})));
    exec(s, "shape.rename", json!({"id": id.0, "name": text}));
    id
}

/// A deck built with `build` on a new presentation, as `.pptx` bytes.
fn deck_bytes(build: impl FnOnce(&mut Session)) -> Vec<u8> {
    let mut s = Session::with_new();
    build(&mut s);
    save_bytes(&s.active().expect("doc").doc, "pptx").expect("pptx")
}

/// Three slides; the first holds the boxes "alpha", "beta" and "gamma" and the connector "link"
/// from alpha to beta.
fn basic(s: &mut Session) {
    let a = add_box(s, "alpha", [60.0, 60.0, 200.0, 80.0]);
    let b = add_box(s, "beta", [320.0, 60.0, 200.0, 80.0]);
    add_box(s, "gamma", [60.0, 260.0, 200.0, 80.0]);
    let link = id_of(&exec(s, "shape.connect", json!({"from": a.0, "to": b.0})));
    exec(s, "shape.rename", json!({"id": link.0, "name": "link"}));
    exec(s, "slide.new", json!({"layout": "titleAndContent", "title": "Two", "body": "second"}));
    exec(s, "slide.new", json!({"layout": "titleOnly", "title": "Three"}));
    exec(s, "slide.go", json!({"index": 0}));
}

/// One editor and the worker's document service in a room on `bytes`.
struct Rig {
    relay: MemRelay,
    live: DeckLive<MemHub>,
    session: Session,
    doc: DeckWorkerDoc<MemHub>,
}

impl Rig {
    fn new(bytes: &[u8]) -> Rig {
        let relay = MemRelay::new();
        let hub = relay.connect("alice", "Alice", true);
        let mut session = Session::new();
        let base = BaseFile { bytes, name: NAME, expected_sha256: None };
        let live = DeckLive::start(hub, ROOM, &base, Access::Edit, "alice", &mut session).expect("editor starts");
        let worker_hub = relay.connect(SERVICE_UID, "Document service", true);
        let doc = DeckService.open(worker_hub, ROOM, NAME, bytes).expect("the service opens the deck");
        let mut rig = Rig { relay, live, session, doc };
        rig.settle();
        assert!(rig.live.editable(), "the editor is live");
        rig
    }

    fn settle(&mut self) {
        for _ in 0..64 {
            self.live.pump(&mut self.session);
            self.doc.pump();
            if self.relay.quiet() && !self.live.holding() {
                self.live.pump(&mut self.session);
                self.doc.pump();
                if self.relay.quiet() {
                    return;
                }
            }
        }
    }

    fn run(&mut self, id: &str, params: Value) -> Value {
        let r = self.live.run(&mut self.session, id, &params).unwrap_or_else(|e| panic!("{id}: {e}"));
        self.settle();
        r
    }

    /// The editor's presentation.
    fn deck(&self) -> Arc<Presentation> {
        let uid = self.live.doc();
        self.session.documents().iter().find(|d| d.uid == uid).expect("the live document").doc.clone()
    }

    fn id(&self, name: &str) -> ShapeId {
        let mut found = None;
        for s in &self.deck().slides {
            deckcraft_model::walk(&s.shapes, &mut |sh, _| {
                if found.is_none() && sh.name == name {
                    found = Some(sh.id);
                }
            });
        }
        found.unwrap_or_else(|| panic!("no shape named {name:?}"))
    }

    /// The worker checkpoints the room and writes the checkpoint as the file.
    fn saved(&mut self) -> Vec<u8> {
        let state = self.doc.live_mut().checkpoint().expect("checkpoint");
        self.settle();
        self.doc.save(&state, NAME).expect("the worker saves")
    }
}

/// Numbers to 1/100 pt: the file stores whole EMUs.
fn rounded(v: Value) -> Value {
    match v {
        Value::Number(n) => n.as_f64().map_or(Value::Number(n), |f| json!((f * 100.0).round() / 100.0)),
        Value::Array(a) => Value::Array(a.into_iter().map(rounded).collect()),
        Value::Object(o) => Value::Object(o.into_iter().map(|(k, v)| (k, rounded(v))).collect()),
        v => v,
    }
}

/// Each slide in order, and on it each shape in z-order (tree order) with its name, kind, text,
/// geometry, fill and, for a connector, the names of the shapes its ends are glued to. Ids are
/// not compared: the writer renumbers them; slides are compared by order, shapes by name.
fn finger(p: &Presentation) -> Vec<String> {
    let mut out = Vec::new();
    for (i, s) in p.slides.iter().enumerate() {
        out.push(format!("slide {i} hidden={} shapes={}", s.hidden, s.shapes.len()));
        let mut names = std::collections::HashMap::new();
        deckcraft_model::walk(&s.shapes, &mut |sh, _| {
            names.insert(sh.id, sh.name.clone());
        });
        let end = |e: &Option<(ShapeId, u32)>| match e {
            Some((id, site)) => format!("{}#{site}", names.get(id).map_or("?", String::as_str)),
            None => "-".to_owned(),
        };
        deckcraft_model::walk(&s.shapes, &mut |sh: &Shape, depth| {
            let ends = match &sh.kind {
                ShapeKind::Connector { start, end: e } => format!(" ends={}->{}", end(start), end(e)),
                _ => String::new(),
            };
            out.push(format!(
                "  {depth} {:?} {} text={:?} xfrm={} fill={}{ends}",
                sh.name,
                sh.kind_name(),
                sh.text.as_ref().map(|t| t.text()),
                rounded(serde_json::to_value(sh.xfrm).expect("xfrm")),
                serde_json::to_value(&sh.fill).expect("fill"),
            ));
        });
    }
    out
}

fn reopened(bytes: &[u8]) -> Presentation {
    open_presentation(NAME, bytes).expect("the saved file opens")
}

#[test]
fn accepts_only_pptx() {
    assert!(DeckService.accepts("Deck.pptx"));
    assert!(DeckService.accepts("dir/Deck.PPTX"));
    for name in ["Deck.potx", "Deck.ppsx", "Deck.ppt", "Deck.odp", "Deck.deckcraft", "Deck", ".pptx", "Deck.pptx.zip"] {
        assert!(!DeckService.accepts(name), "{name} is not saved live");
    }
    assert!(DeckService.pinned_base(), "rooms open from the base the app pinned");
}

#[test]
fn saved_file_reopens_equal_to_the_live_deck() {
    let mut rig = Rig::new(&deck_bytes(basic));
    let (a, b, g) = (rig.id("alpha"), rig.id("beta"), rig.id("gamma"));
    rig.run("shape.move", json!({"ids": [a.0], "dx": 40.0, "dy": 20.0}));
    rig.run("shape.fill", json!({"ids": [g.0], "color": "#C00000"}));
    rig.run("arrange.sendToBack", json!({"ids": [g.0]}));
    rig.run("edit.delete", json!({"ids": [b.0]}));
    rig.run("text.edit", json!({"id": a.0}));
    rig.run("text.insert", json!({"text": "Z"}));
    rig.run("edit.deselect", json!({}));
    rig.run("slide.move", json!({"from": 2, "to": 0}));

    let want = finger(&rig.deck());
    let joined = want.join("\n");
    assert!(want.iter().any(|l| l.contains("\"gamma\"") && l.contains("\"r\":192")), "the restyle is in the comparison:\n{joined}");
    assert!(joined.contains("Z"), "the typing is in the comparison:\n{joined}");
    assert!(!joined.contains("\"beta\""), "the delete is in the comparison:\n{joined}");
    assert!(joined.contains("ends=alpha#") && joined.contains("->-"), "the connector lost its deleted end:\n{joined}");
    let first = &rig.deck().slides[0];
    assert!(first.shapes.iter().any(|s| s.text.as_ref().is_some_and(|t| t.text() == "Three")), "the third slide moved to the front");

    let saved = rig.saved();
    let got = finger(&reopened(&saved));
    assert_eq!(got, want, "the saved file shows what the editor shows");
}

#[test]
fn saved_connector_between_two_live_minted_shapes_stays_connected() {
    let mut rig = Rig::new(&deck_bytes(basic));
    let left = id_of(&rig.run("shape.insert", json!({"preset": "rect", "rect": [100.0, 380.0, 120.0, 60.0]})));
    rig.run("shape.rename", json!({"id": left.0, "name": "left"}));
    let right = id_of(&rig.run("shape.insert", json!({"preset": "ellipse", "rect": [500.0, 380.0, 120.0, 60.0]})));
    rig.run("shape.rename", json!({"id": right.0, "name": "right"}));
    let joint = id_of(&rig.run("shape.connect", json!({"from": left.0, "to": right.0})));
    rig.run("shape.rename", json!({"id": joint.0, "name": "joint"}));
    for id in [left, right, joint] {
        assert!(id.0 > i32::MAX as u32 && id.0 >= LIVE_ID_BASE, "{:#x} is minted live", id.0);
    }

    let editor = rig.deck();
    let kind = |p: &Presentation| p.slides[0].shapes.iter().find(|s| s.name == "joint").map(|s| s.kind.clone());
    assert!(
        matches!(kind(&editor), Some(ShapeKind::Connector { start: Some(_), end: Some(_) })),
        "the editor holds the connector: {:?}",
        kind(&editor)
    );
    let rebuilt = deckcraft_live::deck_from_doc(rig.live.base(), rig.live.replica().expect("replica"));
    assert!(
        matches!(kind(&rebuilt), Some(ShapeKind::Connector { start: Some(_), end: Some(_) })),
        "the room holds the connector: {:?}",
        kind(&rebuilt)
    );
    let saved = reopened(&rig.saved());
    let slide = &saved.slides[0];
    let named = |name: &str| slide.shapes.iter().find(|s| s.name == name).unwrap_or_else(|| panic!("{name} is saved"));
    let ShapeKind::Connector { start: Some((from, _)), end: Some((to, _)) } = named("joint").kind else {
        panic!("the connector keeps both ends: {:?}", named("joint").kind)
    };
    assert_eq!((from, to), (named("left").id, named("right").id), "its ends name the two shapes");
    assert_eq!(finger(&saved), finger(&rig.deck()), "and the rest of the file is the editor's");
}

#[test]
fn save_keeps_pictures_and_unshared_parts_from_the_base() {
    let bytes = deck_bytes(|s| {
        basic(s);
        exec(s, "insert.picture", json!({"data": deckcraft_engine::cmd::base64_encode(PNG), "name": "red.png", "rect": [600.0, 300.0, 80.0, 80.0]}));
        let g = add_box(s, "anim", [400.0, 400.0, 100.0, 50.0]);
        exec(s, "animation.add", json!({"effect": "fade", "ids": [g.0]}));
        exec(s, "comment.add", json!({"text": "keep this comment"}));
    });
    let base = reopened(&bytes);
    let pictures = |p: &Presentation| -> Vec<Vec<u8>> {
        let mut v: Vec<Vec<u8>> = p.media.iter().map(|m| m.data.to_vec()).collect();
        v.sort();
        v
    };
    let parts = |p: &Presentation| {
        let s = &p.slides[0];
        (
            s.shapes.iter().filter(|sh| matches!(sh.kind, ShapeKind::Picture { .. })).count(),
            s.animations.len(),
            s.comments.iter().map(|c| c.text.clone()).collect::<Vec<_>>(),
        )
    };
    assert!(pictures(&base).iter().any(|d| d == PNG), "the base holds the picture");
    let (pics, anims, comments) = parts(&base);
    assert!(pics == 1 && anims == 1 && comments == ["keep this comment"], "the base holds the parts: {:?}", parts(&base));

    let mut rig = Rig::new(&bytes);
    let a = rig.id("alpha");
    rig.run("text.edit", json!({"id": a.0}));
    rig.run("text.insert", json!({"text": "Q"}));
    rig.run("edit.deselect", json!({}));
    let saved = reopened(&rig.saved());
    assert_eq!(pictures(&saved), pictures(&base), "the picture's bytes are saved");
    assert_eq!(parts(&saved), parts(&base), "the picture, the animation and the comment are saved");
    assert_eq!(finger(&saved), finger(&rig.deck()), "with the editor's typing");
    assert!(finger(&saved).join("\n").contains("Q"), "the typing is in the comparison");
}

#[test]
fn an_unchanged_room_saves_nothing() {
    let mut rig = Rig::new(&deck_bytes(basic));
    let before = rig.doc.snapshot();
    rig.settle();
    let idle = rig.doc.snapshot();
    assert!(rig.doc.unchanged(&before, &idle), "pumping an idle room replaces no slide");
    assert!(rig.doc.same(&before, &idle));
    let a = rig.id("alpha");
    rig.run("shape.move", json!({"ids": [a.0], "dx": 10.0}));
    let after = rig.doc.snapshot();
    assert!(!rig.doc.unchanged(&before, &after), "an edit replaces a slide");
    assert!(!rig.doc.same(&before, &after), "and the content differs");
}

/// D2 review Minor 5: a deck holding live-minted ids, kept as a native file (a recovered copy),
/// reopens without a live session and still grows below the live range.
#[test]
fn a_recovered_native_deck_with_live_ids_still_grows() {
    let mut rig = Rig::new(&deck_bytes(basic));
    let live_id = id_of(&rig.run("shape.insert", json!({"preset": "rect", "rect": [100.0, 380.0, 120.0, 60.0]})));
    assert!(live_id.0 >= LIVE_ID_BASE);
    let native = save_bytes(&rig.deck(), "deckcraft").expect("native");
    let deck = open_presentation("Recovered.deckcraft", &native).expect("reopens");
    assert!(deck.next_id < LIVE_ID_BASE, "the counter stays below the live range");
    let mut s = Session::new();
    add_opened(&mut s, "Recovered.deckcraft", None, deck);
    let mut ids = std::collections::HashSet::new();
    for s in &s.active().expect("doc").doc.slides {
        deckcraft_model::walk(&s.shapes, &mut |sh, _| {
            ids.insert(sh.id);
        });
    }
    assert!(ids.contains(&live_id), "the live-minted shape is kept");
    for _ in 0..3 {
        let id = id_of(&exec(&mut s, "shape.insert", json!({"preset": "rect", "rect": [10.0, 10.0, 40.0, 40.0]})));
        assert!(id.0 < LIVE_ID_BASE, "a new id {:#x} is below the live range", id.0);
        assert!(ids.insert(id), "and unused");
    }
}

/// A worker whose replica fell behind for good (another client's updates wait on one that never
/// reaches the room) ends its link as Broken, the status on which craft-live-worker stops saving,
/// tells the editors to reopen and leaves the file at its last good save. Nothing after the break
/// is taken in: the worker's slides stay those of that save.
#[test]
fn a_worker_that_fell_behind_for_good_reports_broken() {
    use craft_collab::{CRDT_TEXT, ClientMsg, Hub, Replica, TextPayload};
    use craft_live::LiveStatus;
    use deckcraft_live::DeckDoc;

    let mut rig = Rig::new(&deck_bytes(basic));
    let a = rig.id("alpha");
    rig.run("shape.move", json!({"ids": [a.0], "dx": 10.0}));
    let good = rig.doc.snapshot();
    let mut rogue = DeckDoc::from_state(&rig.live.replica().expect("replica").state()).expect("state");
    rogue.set_client(30_000).expect("space");
    let mut write = |s: String| {
        rogue.write(false, |t| {
            t.set_shape(a, "name", json!(s));
            Ok(())
        })
    };
    let _lost = write("first".into()).expect("first");
    let later: Vec<Vec<u8>> = (0..4).map(|i| write(format!("{i}").repeat(400 * 1024)).expect("later")).collect();
    let mut hub = rig.relay.connect("rogue", "Rogue", true);
    for (i, u) in later.iter().enumerate() {
        let params = serde_json::to_value(TextPayload::new(u)).expect("payload");
        hub.send(ClientMsg::Submit { doc: ROOM.into(), local: i as u64 + 1, command: CRDT_TEXT.into(), params, scope: None, fence: None })
            .expect("send");
    }
    for _ in 0..4 {
        rig.live.pump(&mut rig.session);
        rig.doc.pump();
    }
    assert!(matches!(rig.doc.live().status(), LiveStatus::Broken(_)), "the worker's link ended: {:?}", rig.doc.live().status());
    assert!(rig.doc.same(&good, &rig.doc.snapshot()), "the worker holds the last good deck");
}
