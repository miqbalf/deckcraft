//! Presence: where each editor is, sent in stable ids with sticky text carets, and drawn where
//! that content is now (L7).

use craft_collab::{ClientMsg, Hub, ServerMsg};
use craft_live::testing::ROOM;
use craft_live::{Access, CURSOR_KIND, SERVICE_UID};
use deckcraft_engine::DocState;
use deckcraft_live::schema::{hex, id_key};
use deckcraft_live::{DeckPos, Peer, TextTarget, Tomb, presence};
use deckcraft_model::ShapeId;
use serde_json::json;

use crate::support::{Room, add_box, basic, deck_bytes, find, shape_of};

/// Peer `i`'s live document.
fn state(r: &Room, i: usize) -> &DocState {
    let p = &r.peers[i];
    p.session.documents().iter().find(|d| d.uid == p.live.doc()).expect("live doc")
}

/// Who peer `i` sees after one more pump.
fn seen(r: &mut Room, i: usize) -> Vec<Peer> {
    let p = &mut r.peers[i];
    p.live.pump(&mut p.session)
}

/// Where peer `i` draws peer `of`; None when it draws no caret for it.
fn caret_of(r: &mut Room, i: usize, of: usize) -> Option<DeckPos> {
    let id = r.relay.peer_id(r.peers[of].live.core().hub());
    let peers = seen(r, i);
    assert!(peers.iter().any(|p| p.id == id), "peer {of} is in peer {i}'s roster");
    peers.into_iter().find(|p| p.id == id).and_then(|p| p.caret)
}

/// Peer `i` tells the room where it is, past the caret limit, and the room settles.
fn share(r: &mut Room, i: usize) {
    r.tick(250);
    let p = &mut r.peers[i];
    p.live.send_selection(&p.session);
    r.settle();
}

fn text_of(r: &Room, peer: usize, id: ShapeId) -> String {
    shape_of(&r.peers[peer].doc(), id).and_then(|s| s.text).map(|t| t.text()).unwrap_or_default()
}

/// Peer `i`'s own text caret: (paragraph, char).
fn my_caret(r: &Room, i: usize) -> (usize, usize) {
    state(r, i).selection.text.as_ref().map(|t| t.caret).expect("a text caret")
}

#[test]
fn payload_round_trips_and_stays_under_16_kib() {
    let bytes = deck_bytes(|s| {
        for i in 0..1000 {
            add_box(s, "x", [f64::from(i % 40) * 12.0, f64::from(i / 40) * 12.0, 10.0, 10.0]);
        }
    });
    let mut r = Room::new(bytes, 2, 41);
    let ids: Vec<u32> = state(&r, 1).doc.slides[0].shapes.iter().map(|s| s.id.0).collect();
    assert!(ids.len() >= 1000, "the deck holds the 1,000 shapes");
    r.peers[1].run("edit.select", json!({"ids": ids})).expect("select");
    let st = state(&r, 1);
    assert!(st.selection.shapes.len() >= 1000, "all of them are selected");
    let sent = presence::payload(st, r.peers[1].live.replica()).expect("a payload");
    let size = serde_json::to_vec(&sent).expect("json").len();
    assert!(size < 16 * 1024, "the payload is {size} bytes");
    let back = presence::read(&sent).expect("it reads back");
    assert_eq!(back.slide, st.doc.slides[st.selection.slide].id, "the slide round-trips");
    assert_eq!(back.shapes.len(), presence::SHAPES_MAX, "at most 64 shapes are sent");
    assert_eq!(back.shapes, st.selection.shapes[..presence::SHAPES_MAX].to_vec(), "the first 64, in order");
    assert!(back.text.is_none(), "no text caret");

    // A text caret round-trips as sticky indexes into the same text.
    let first = find(&r.peers[1].doc(), "x");
    r.peers[1].run("text.edit", json!({"id": first.0, "at": [0, 1]})).expect("edit");
    let sent = presence::payload(state(&r, 1), r.peers[1].live.replica()).expect("a payload");
    let back = presence::read(&sent).expect("it reads back");
    let t = back.text.expect("a text caret");
    assert_eq!(t.target, TextTarget::Shape(first));
    let at = u32::try_from(my_caret(&r, 1).1).ok();
    assert!(at.is_some_and(|c| c > 0), "the caret is inside the text");
    let replica = r.peers[1].live.replica().expect("replica");
    assert_eq!(replica.resolve(t.target.clone(), &t.head), at, "the head is where the caret is");
    assert_eq!(replica.resolve(t.target.clone(), &t.anchor), at, "so is the anchor");

    // Peer 0 draws peer 1's first 64 shapes.
    r.peers[1].run("edit.select", json!({"ids": ids})).expect("select");
    share(&mut r, 1);
    let at = caret_of(&mut r, 0, 1).expect("peer 1's caret");
    assert_eq!(at.shapes, state(&r, 1).selection.shapes[..presence::SHAPES_MAX].to_vec());
}

#[test]
fn my_caret_stays_on_my_characters_through_remote_typing() {
    for seed in 1..4 {
        let mut r = Room::new(basic(), 2, seed);
        let a = find(&r.peers[0].doc(), "alpha");
        // Peer 0's caret sits before the "l" of "alpha".
        r.peers[0].run("text.edit", json!({"id": a.0, "at": [0, 1]})).expect("edit");
        r.settle();
        // Peer 1 types an "a" at the very start: "aalpha". A diff of the text alone cannot tell
        // which "a" is new.
        r.peers[1].run("text.edit", json!({"id": a.0, "at": [0, 0]})).expect("edit");
        r.peers[1].run("text.insert", json!({"text": "a"})).expect("type");
        r.settle();
        let text: Vec<char> = text_of(&r, 0, a).chars().collect();
        let (_, c) = my_caret(&r, 0);
        assert_eq!(text.get(c), Some(&'l'), "seed {seed}: peer 0's caret is still before its \"l\" in {text:?} (at {c})");
        r.peers[0].run("text.insert", json!({"text": "Z"})).expect("type");
        r.settle();
        for i in 0..2 {
            assert!(text_of(&r, i, a).contains("Zl"), "seed {seed}: peer 0 typed before its \"l\": {:?}", text_of(&r, i, a));
        }
        r.assert_converged();
    }
}

#[test]
fn remote_caret_stays_on_its_characters_through_my_typing() {
    let mut r = Room::new(basic(), 2, 9);
    let a = find(&r.peers[0].doc(), "alpha");
    r.peers[1].run("text.edit", json!({"id": a.0, "at": [0, 2]})).expect("edit");
    share(&mut r, 1);
    let before = caret_of(&mut r, 0, 1).and_then(|p| p.caret).expect("peer 1's text caret");
    assert_eq!(before.target, TextTarget::Shape(a));
    assert_eq!(before.head, (0, 2), "drawn where peer 1 put it");
    // Peer 0 types two characters in front of it; peer 1 sends nothing new.
    r.peers[0].run("text.edit", json!({"id": a.0, "at": [0, 0]})).expect("edit");
    r.peers[0].run("text.insert", json!({"text": "XY"})).expect("type");
    r.settle();
    let after = caret_of(&mut r, 0, 1).and_then(|p| p.caret).expect("peer 1's text caret");
    let text: Vec<char> = text_of(&r, 0, a).chars().collect();
    assert_eq!(text.get(after.head.1), Some(&'p'), "peer 1's caret is still before the \"p\" of {text:?}");
    assert_eq!(after.anchor, after.head, "a caret, not a range");
}

#[test]
fn remote_caret_follows_a_slide_reorder() {
    let mut r = Room::new(basic(), 2, 5);
    r.peers[1].run("slide.go", json!({"index": 2})).expect("go");
    share(&mut r, 1);
    let third = r.peers[0].slide_ids()[2];
    assert_eq!(caret_of(&mut r, 0, 1).and_then(|p| p.slide), Some(2), "peer 1 is drawn on the third slide");
    r.peers[0].run("slide.move", json!({"from": 2, "to": 0})).expect("move");
    let now = r.peers[0].slide_ids().iter().position(|id| *id == third);
    assert_eq!(now, Some(0), "the slide moved to the front");
    assert_eq!(caret_of(&mut r, 0, 1).and_then(|p| p.slide), now, "peer 1 is drawn on the same slide, now first");
    r.settle();
    assert_eq!(caret_of(&mut r, 0, 1).and_then(|p| p.slide), now, "and stays there");
}

#[test]
fn caret_on_an_unknown_shape_is_hidden_then_shown() {
    let mut r = Room::new(basic(), 3, 11);
    let alpha = find(&r.peers[0].doc(), "alpha");
    let slide = r.peers[0].slide_ids()[0];
    // Peer 1 adds a shape while its link is down: peer 0 does not know it yet.
    r.drop_link(1);
    r.pump_all();
    r.peers[1].run("shape.insert", json!({"preset": "rect", "rect": [400.0, 300.0, 120.0, 60.0], "text": "fresh"})).expect("insert");
    let fresh = find(&r.peers[1].doc(), "fresh");
    assert!(shape_of(&r.peers[0].doc(), fresh).is_none(), "peer 0 does not know the new shape");
    let sticky = r.peers[1].live.replica().and_then(|d| d.sticky(TextTarget::Shape(fresh), 2)).expect("a sticky index");
    // Peer 2 says it has the new shape and alpha selected, its caret in the new shape's text.
    let payload = json!({
        "s": id_key(slide.0),
        "sh": [id_key(fresh.0), id_key(alpha.0)],
        "t": {"k": "s", "id": id_key(fresh.0), "a": hex(&sticky), "h": hex(&sticky)},
    });
    let origin = r.relay.peer_id(r.peers[2].live.core().hub());
    r.relay.inject(r.peers[0].live.core().hub(), ServerMsg::Ephemeral { doc: ROOM.into(), origin, kind: CURSOR_KIND.into(), payload });
    let hidden = caret_of(&mut r, 0, 2).expect("peer 2's caret is kept");
    assert_eq!(hidden.slide, Some(0), "drawn on its slide");
    assert_eq!(hidden.shapes, vec![alpha], "the unknown shape is not drawn");
    assert_eq!(hidden.caret, None, "nor the caret in its text");
    // The shape reaches peer 0.
    r.restore_link(1);
    r.settle();
    assert!(shape_of(&r.peers[0].doc(), fresh).is_some(), "peer 0 has the new shape");
    let shown = caret_of(&mut r, 0, 2).expect("peer 2's caret");
    assert_eq!(shown.shapes, vec![fresh, alpha], "now the new shape is drawn too");
    let c = shown.caret.expect("and the caret in its text");
    assert_eq!(c.target, TextTarget::Shape(fresh));
    assert_eq!(c.head, (0, 2), "where the sticky index points");
}

/// Peer 1 moves gamma, so every peer takes in one more remote change (and follows the carets).
fn nudge(r: &mut Room) {
    let gamma = find(&r.peers[1].doc(), "gamma");
    r.peers[1].run("slide.go", json!({"index": 0})).expect("go");
    r.peers[1].run("shape.move", json!({"ids": [gamma.0], "dx": 5.0})).expect("move");
    r.settle();
}

#[test]
fn caret_on_a_deleted_shape_is_dropped() {
    let mut r = Room::new(basic(), 2, 6);
    let doc = r.peers[0].doc();
    let (alpha, beta) = (find(&doc, "alpha"), find(&doc, "beta"));
    r.peers[1].run("edit.select", json!({"ids": [alpha.0, beta.0]})).expect("select");
    share(&mut r, 1);
    assert_eq!(caret_of(&mut r, 0, 1).map(|p| p.shapes), Some(vec![alpha, beta]));
    r.peers[0].run("edit.delete", json!({"ids": [beta.0]})).expect("delete");
    assert_eq!(caret_of(&mut r, 0, 1).map(|p| p.shapes), Some(vec![alpha]), "the deleted shape is not drawn");
    // Brought back, it is not drawn as selected again: the caret dropped it for good.
    {
        let p = &mut r.peers[0];
        p.live.restore(&mut p.session, Tomb::Shape(beta)).expect("restore");
    }
    r.settle();
    nudge(&mut r);
    assert!(shape_of(&r.peers[0].doc(), beta).is_some(), "beta is back");
    assert_eq!(caret_of(&mut r, 0, 1).map(|p| p.shapes), Some(vec![alpha]), "the dropped shape stays dropped");

    // A text caret in a deleted shape is dropped the same way.
    r.peers[1].run("text.edit", json!({"id": beta.0, "at": [0, 1]})).expect("edit");
    share(&mut r, 1);
    assert_eq!(caret_of(&mut r, 0, 1).and_then(|p| p.caret).map(|c| c.target), Some(TextTarget::Shape(beta)));
    r.peers[0].run("edit.delete", json!({"ids": [beta.0]})).expect("delete");
    assert_eq!(caret_of(&mut r, 0, 1).and_then(|p| p.caret), None, "no caret in a deleted shape");
    {
        let p = &mut r.peers[0];
        p.live.restore(&mut p.session, Tomb::Shape(beta)).expect("restore");
    }
    r.settle();
    nudge(&mut r);
    assert_eq!(caret_of(&mut r, 0, 1).and_then(|p| p.caret), None, "and none after it is back");
}

#[test]
fn caret_on_a_deleted_slide_is_dropped() {
    let mut r = Room::new(basic(), 2, 12);
    r.peers[1].run("slide.go", json!({"index": 1})).expect("go");
    share(&mut r, 1);
    let second = r.peers[0].slide_ids()[1];
    assert_eq!(caret_of(&mut r, 0, 1).and_then(|p| p.slide), Some(1));
    r.peers[0].run("slide.go", json!({"index": 1})).expect("go");
    r.peers[0].run("edit.delete", json!({"scope": "slides"})).expect("delete");
    assert!(!r.peers[0].slide_ids().contains(&second), "the slide is gone");
    assert_eq!(caret_of(&mut r, 0, 1), None, "peer 1 is drawn on no slide");
    {
        let p = &mut r.peers[0];
        p.live.restore(&mut p.session, Tomb::Slide(second)).expect("restore");
    }
    r.settle();
    nudge(&mut r);
    assert!(r.peers[0].slide_ids().contains(&second), "the slide is back");
    assert_eq!(caret_of(&mut r, 0, 1), None, "the dropped caret stays dropped");
}

#[test]
fn roster_leaves_out_the_service_peer() {
    let mut r = Room::new(basic(), 2, 7);
    let slide = r.peers[0].slide_ids()[0];
    let mut service = r.relay.connect(SERVICE_UID, "Document service", true);
    service
        .send(ClientMsg::Ephemeral { doc: ROOM.into(), kind: CURSOR_KIND.into(), payload: json!({"s": id_key(slide.0), "sh": []}) })
        .expect("send");
    r.settle();
    let other = r.relay.peer_id(r.peers[1].live.core().hub());
    let peers = seen(&mut r, 0);
    assert!(peers.iter().all(|p| craft_live::uid_of(&p.id) != SERVICE_UID), "the service is not listed: {peers:?}");
    assert_eq!(peers.iter().map(|p| p.id.clone()).collect::<Vec<_>>(), vec![other], "only the other editor is");
}

#[test]
fn read_only_peer_still_sends_its_selection() {
    let mut r = Room::with_access(basic(), &[Access::Edit, Access::Read], 8);
    assert!(!r.peers[1].live.editable(), "peer 1 only reads");
    let alpha = find(&r.peers[1].doc(), "alpha");
    r.peers[1].run("edit.select", json!({"ids": [alpha.0]})).expect("select");
    share(&mut r, 1);
    assert_eq!(caret_of(&mut r, 0, 1).map(|p| p.shapes), Some(vec![alpha]), "the reader's selection is drawn");
    r.peers[1].run("text.edit", json!({"id": alpha.0, "at": [0, 3]})).expect("edit");
    share(&mut r, 1);
    let c = caret_of(&mut r, 0, 1).and_then(|p| p.caret).expect("the reader's caret");
    assert_eq!((c.target, c.head), (TextTarget::Shape(alpha), (0, 3)));
}
