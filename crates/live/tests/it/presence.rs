//! Presence: where each editor is, sent in stable ids with sticky text carets, and drawn where
//! that content is now (L7).

use craft_collab::{ClientMsg, Hub, ServerMsg};
use craft_live::testing::ROOM;
use craft_live::{Access, CURSOR_KIND, SERVICE_UID};
use deckcraft_engine::{DocState, PointerKind};
use deckcraft_live::schema::{hex, id_key};
use deckcraft_live::{DeckPos, Peer, TextTarget, Tomb, presence};
use deckcraft_model::ShapeId;
use serde_json::json;

use crate::support::{Room, add_box, basic, deck_bytes, ev, find, shape_of};

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

/// The character right after `(paragraph, char)` in shape `id`'s one-paragraph text on `peer`.
fn char_at(r: &Room, peer: usize, id: ShapeId, (_, c): (usize, usize)) -> Option<char> {
    text_of(r, peer, id).chars().nth(c)
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
    assert_eq!(char_at(&r, 0, a, before.head), char_at(&r, 1, a, my_caret(&r, 1)), "drawn before the character peer 1's caret is before");
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
    let want = text_of(&r, 1, fresh).chars().nth(2);
    assert_eq!(char_at(&r, 0, fresh, c.head), want, "before the character the sticky index was taken before");
}

/// Peer 1 moves gamma, so every peer takes in one more remote change (and follows the carets).
fn nudge(r: &mut Room) {
    let gamma = find(&r.peers[1].doc(), "gamma");
    r.peers[1].run("slide.go", json!({"index": 0})).expect("go");
    r.peers[1].run("shape.move", json!({"ids": [gamma.0], "dx": 5.0})).expect("move");
    r.settle();
}

#[test]
fn caret_on_a_deleted_shape_is_hidden_until_it_is_restored() {
    let mut r = Room::new(basic(), 2, 6);
    let doc = r.peers[0].doc();
    let (alpha, beta) = (find(&doc, "alpha"), find(&doc, "beta"));
    r.peers[1].run("edit.select", json!({"ids": [alpha.0, beta.0]})).expect("select");
    share(&mut r, 1);
    assert_eq!(caret_of(&mut r, 0, 1).map(|p| p.shapes), Some(vec![alpha, beta]));
    r.peers[0].run("edit.delete", json!({"ids": [beta.0]})).expect("delete");
    assert_eq!(caret_of(&mut r, 0, 1).map(|p| p.shapes), Some(vec![alpha]), "the deleted shape is not drawn");
    // Brought back, it is drawn as peer 1's again, though peer 1 sent nothing new.
    {
        let p = &mut r.peers[0];
        p.live.restore(&mut p.session, Tomb::Shape(beta)).expect("restore");
    }
    r.settle();
    nudge(&mut r);
    assert!(shape_of(&r.peers[0].doc(), beta).is_some(), "beta is back");
    assert_eq!(caret_of(&mut r, 0, 1).map(|p| p.shapes), Some(vec![alpha, beta]), "the restored shape is drawn again");

    // A text caret in a deleted shape is hidden the same way.
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
    assert_eq!(caret_of(&mut r, 0, 1).and_then(|p| p.caret).map(|c| c.target), Some(TextTarget::Shape(beta)), "the caret is back with the shape");
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
    assert_eq!(c.target, TextTarget::Shape(alpha));
    assert_eq!(char_at(&r, 0, alpha, c.head), char_at(&r, 1, alpha, my_caret(&r, 1)), "before the reader's character");
}

#[test]
fn caret_at_the_end_of_a_text_agrees_with_its_owner() {
    let mut r = Room::new(basic(), 2, 13);
    let a = find(&r.peers[0].doc(), "alpha");
    let end = text_of(&r, 1, a).chars().count();
    r.peers[1].run("text.edit", json!({"id": a.0, "at": [0, end]})).expect("edit");
    share(&mut r, 1);
    let drawn = caret_of(&mut r, 0, 1).and_then(|p| p.caret).expect("peer 1's caret");
    assert_eq!(drawn.head, my_caret(&r, 1), "drawn where peer 1 has it, at the end");
    // Peer 0 appends right at peer 1's caret; peer 1 sends nothing new.
    r.peers[0].run("text.edit", json!({"id": a.0, "at": [0, end]})).expect("edit");
    r.peers[0].run("text.insert", json!({"text": "Zed"})).expect("type");
    r.settle();
    let mine = my_caret(&r, 1);
    let drawn = caret_of(&mut r, 0, 1).and_then(|p| p.caret).expect("peer 1's caret");
    assert!(mine.1 <= text_of(&r, 1, a).chars().count(), "peer 1's caret is inside its text");
    assert_eq!(drawn.head, mine, "peer 0 draws peer 1's caret on the side peer 1's own caret went");
    r.assert_converged();
}

#[test]
fn payload_sends_no_text_caret_from_a_text_the_replica_differs_on() {
    let mut r = Room::new(basic(), 2, 14);
    let a = find(&r.peers[0].doc(), "alpha");
    r.peers[0].run("text.edit", json!({"id": a.0, "at": [0, 3]})).expect("edit");
    // Peer 1's replica has typing in alpha that peer 0's engine has not seen.
    r.peers[1].run("text.edit", json!({"id": a.0, "at": [0, 0]})).expect("edit");
    r.peers[1].run("text.insert", json!({"text": "QQ"})).expect("type");
    let sent = presence::payload(state(&r, 0), r.peers[1].live.replica()).expect("a payload");
    let back = presence::read(&sent).expect("it reads back");
    assert!(back.text.is_none(), "offsets of one text are not taken in another: {sent}");
    assert_eq!(back.slide, r.peers[0].slide_ids()[0], "the slide still goes");
}

#[test]
fn remote_caret_is_hidden_while_a_change_to_its_text_is_held() {
    let mut r = Room::new(basic(), 3, 15);
    let doc = r.peers[0].doc();
    let (alpha, gamma) = (find(&doc, "alpha"), find(&doc, "gamma"));
    r.peers[1].run("text.edit", json!({"id": alpha.0, "at": [0, 3]})).expect("edit");
    share(&mut r, 1);
    let want = char_at(&r, 1, alpha, my_caret(&r, 1));
    assert!(caret_of(&mut r, 0, 1).and_then(|p| p.caret).is_some(), "drawn before the hold");
    // Peer 0 drags gamma ([60, 260, 200, 80]); peer 2 types in alpha meanwhile.
    r.peers[0].session.pointer(ev(PointerKind::Down, 160.0, 300.0)).expect("down");
    r.peers[0].session.pointer(ev(PointerKind::Drag, 200.0, 320.0)).expect("drag");
    r.peers[2].run("text.edit", json!({"id": alpha.0, "at": [0, 0]})).expect("edit");
    r.peers[2].run("text.insert", json!({"text": "QQ"})).expect("type");
    for _ in 0..4 {
        r.pump_all();
    }
    assert!(r.peers[0].live.holding(), "the typing waits for the drop");
    let held = caret_of(&mut r, 0, 1).expect("peer 1 is still drawn");
    assert_eq!(held.caret, None, "no caret in a text whose change is held");
    r.peers[0].session.pointer(ev(PointerKind::Up, 220.0, 330.0)).expect("up");
    r.settle();
    let back = caret_of(&mut r, 0, 1).and_then(|p| p.caret).expect("drawn again after the drop");
    assert_eq!(char_at(&r, 0, alpha, back.head), want, "before the same character");
    let _ = gamma;
}

#[test]
fn my_caret_moved_without_an_edit_stays_on_its_characters() {
    let mut r = Room::new(basic(), 2, 16);
    let a = find(&r.peers[0].doc(), "alpha");
    r.peers[0].run("text.edit", json!({"id": a.0, "at": [0, 1]})).expect("edit");
    r.settle();
    // Peer 1 types a "p" before "ha": "alppha", which a text diff places after the first "p".
    r.peers[1].run("text.edit", json!({"id": a.0, "at": [0, 2]})).expect("edit");
    r.peers[1].run("text.insert", json!({"text": "p"})).expect("type");
    // Before its next pump, peer 0 clicks before the "h", with no edit and no flush.
    r.peers[0].session.execute("text.edit", &json!({"id": a.0, "at": [0, 3]})).expect("click");
    assert_eq!(char_at(&r, 0, a, my_caret(&r, 0)), Some('h'));
    r.pump(0);
    assert_eq!(char_at(&r, 0, a, my_caret(&r, 0)), Some('h'), "peer 0's caret is still before its \"h\": {:?}", text_of(&r, 0, a));
    r.settle();
    r.assert_converged();
}

/// Peer 2's presence `payload`, put straight into peer 0's inbox.
fn inject(r: &Room, payload: serde_json::Value) {
    let origin = r.relay.peer_id(r.peers[2].live.core().hub());
    r.relay.inject(r.peers[0].live.core().hub(), ServerMsg::Ephemeral { doc: ROOM.into(), origin, kind: CURSOR_KIND.into(), payload });
}

#[test]
fn hostile_presence_is_ignored() {
    use craft_collab::Replica;
    use craft_collab::yrs::updates::decoder::Decode;
    use craft_collab::yrs::updates::encoder::Encode;
    use craft_collab::yrs::{Assoc, ClientID, ID, StickyIndex};
    use deckcraft_live::DeckDoc;

    let mut r = Room::new(basic(), 3, 17);
    let doc = r.peers[0].doc();
    let (alpha, beta, gamma) = (find(&doc, "alpha"), find(&doc, "beta"), find(&doc, "gamma"));
    let ids = r.peers[0].slide_ids();
    let (s0, s1) = (id_key(ids[0].0), id_key(ids[1].0));
    r.peers[0].run("edit.delete", json!({"ids": [gamma.0]})).expect("delete");
    r.settle();
    let real = r.peers[0].live.replica().and_then(|d| d.sticky(TextTarget::Shape(alpha), 2)).expect("sticky");
    let in_beta = r.peers[0].live.replica().and_then(|d| d.sticky(TextTarget::Shape(beta), 2)).expect("sticky");
    // A sticky index into an item no replica here has: another client's typing, never sent.
    let mut rogue = DeckDoc::from_state(&r.peers[0].live.replica().expect("replica").state()).expect("state");
    rogue.set_client(31_000).expect("space");
    let typed = deckcraft_live::text::Chunk::Text { text: "RR".into(), attrs: Vec::new() };
    rogue
        .write(true, |t| {
            t.text_ops(TextTarget::Shape(alpha), &[deckcraft_live::text::TextOp::Insert { at: 0, chunks: vec![typed] }]);
            Ok(())
        })
        .expect("rogue typing");
    let foreign = rogue.sticky(TextTarget::Shape(alpha), 1).expect("a sticky index on the rogue's typing");
    // A known client, a clock far past its end.
    let id = StickyIndex::decode_v1(&real).ok().and_then(|s| s.id().copied()).expect("a relative index");
    let past = StickyIndex::from_id(ID::new(id.client, id.clock + 1_000_000), Assoc::After).encode_v1();
    let unknown = StickyIndex::from_id(ID::new(ClientID::new(987_654_321), 5), Assoc::Before).encode_v1();

    let t = |k: serde_json::Value, id: &str, a: serde_json::Value, h: serde_json::Value| json!({"k": k, "id": id, "a": a, "h": h});
    let ok = json!(hex(&real));
    let mut junk: Vec<serde_json::Value> = (0..10_000)
        .map(|i| match i % 5 {
            0 => json!(i),
            1 => json!(null),
            2 => json!("zz!"),
            3 => json!({"id": i}),
            _ => json!(id_key(alpha.0)),
        })
        .collect();
    junk.push(json!(id_key(beta.0)));
    let hostile: Vec<serde_json::Value> = vec![
        json!(42),
        json!("cursor"),
        json!([1, 2, 3]),
        json!({"s": 7}),
        json!({"s": "zzzzzzzzzz"}),
        json!({"s": "-1"}),
        json!({"s": s0, "sh": junk}),
        json!({"s": s0, "sh": "alpha"}),
        json!({"s": s0, "t": t(json!("x"), &id_key(alpha.0), ok.clone(), ok.clone())}),
        json!({"s": s0, "t": t(json!(1), &id_key(alpha.0), ok.clone(), ok.clone())}),
        json!({"s": s0, "t": t(json!("s"), &id_key(alpha.0), json!("abc"), json!("abc"))}),
        json!({"s": s0, "t": t(json!("s"), &id_key(alpha.0), json!("zz"), json!("zz"))}),
        json!({"s": s0, "t": t(json!("s"), &id_key(alpha.0), json!("éé"), json!("éé"))}),
        json!({"s": s0, "t": t(json!("s"), &id_key(alpha.0), json!(""), json!(""))}),
        json!({"s": s0, "t": t(json!("s"), &id_key(alpha.0), json!("a".repeat(129)), json!("a".repeat(129)))}),
        json!({"s": s0, "t": t(json!("s"), &id_key(alpha.0), json!("ab".repeat(65)), json!("ab".repeat(65)))}),
        json!({"s": s0, "t": t(json!("s"), &id_key(alpha.0), json!("00".repeat(40)), json!("ff".repeat(40)))}),
        json!({"s": s0, "t": t(json!("s"), &id_key(alpha.0), json!(hex(&foreign)), json!(hex(&foreign)))}),
        json!({"s": s0, "t": t(json!("s"), &id_key(alpha.0), json!(hex(&past)), json!(hex(&past)))}),
        json!({"s": s0, "t": t(json!("s"), &id_key(alpha.0), json!(hex(&unknown)), json!(hex(&unknown)))}),
        json!({"s": s0, "t": t(json!("s"), &id_key(alpha.0), json!(hex(&in_beta)), json!(hex(&in_beta)))}),
        json!({"s": s0, "t": t(json!("n"), &s1, ok.clone(), ok.clone())}),
        json!({"s": s0, "t": t(json!("s"), "zzzzzzzzzz", ok.clone(), ok.clone())}),
        json!({"s": s0, "sh": [id_key(gamma.0)], "t": t(json!("s"), &id_key(gamma.0), ok.clone(), ok.clone())}),
        json!({"s": id_key(gamma.0), "sh": [id_key(alpha.0)]}),
    ];
    let before = r.peers[0].doc();
    let rebuilt = r.peers[0].rebuilt();
    let seq = r.relay.seq();
    for (n, payload) in hostile.into_iter().enumerate() {
        inject(&r, payload.clone());
        let drawn = caret_of(&mut r, 0, 2);
        let shown = r.peers[0].doc();
        if let Some(at) = &drawn {
            let slide = at.slide.and_then(|i| shown.slides.get(i));
            assert!(at.shapes.len() <= presence::SHAPES_MAX, "payload {n}: at most 64 shapes drawn");
            assert!(at.shapes.iter().all(|id| slide.is_some_and(|s| s.shape(*id).is_some())), "payload {n}: only shapes on its slide");
            assert_eq!(at.caret, None, "payload {n}: no caret from a bad text caret");
        }
        assert!(*r.peers[0].doc() == *before, "payload {n}: the deck is unchanged");
        assert!(r.peers[0].rebuilt() == rebuilt, "payload {n}: the replica is unchanged");
        assert_eq!(r.relay.seq(), seq, "payload {n}: nothing was sent");
        assert!(r.peers[0].live.editable(), "payload {n}: the link is still live");
    }
    // The good one still works afterwards.
    inject(&r, json!({"s": s0, "sh": [id_key(alpha.0)], "t": t(json!("s"), &id_key(alpha.0), ok.clone(), ok)}));
    let at = caret_of(&mut r, 0, 2).expect("drawn");
    assert_eq!(at.shapes, vec![alpha]);
    assert!(at.caret.is_some(), "a good caret is drawn");
}
