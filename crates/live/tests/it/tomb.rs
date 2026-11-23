//! Tombstones (L5): delete beats edit, Restore, a drop on a deleted shape, remote deletes cleaned
//! by the engine's one routine, and a peer's junk that must not revive a tombstone.

use std::sync::Arc;

use craft_collab::{Hub, Replica};
use deckcraft_engine::{PointerKind, Session};
use deckcraft_live::{DeckDoc, DeckSeed, Tomb, deck_from_doc, deck_from_state, deleted_slide_text, deleted_text};
use deckcraft_model::{Presentation, ShapeId, ShapeKind, SlideId};
use serde_json::json;

use crate::support::{NAME, Room, add_box, basic, deck_bytes, ev, find, shape_of};

fn text_of(r: &Room, peer: usize, id: ShapeId) -> Option<String> {
    shape_of(&r.peers[peer].doc(), id).map(|s| s.text.map(|t| t.text()).unwrap_or_default())
}

/// B deletes the shape A types in, at the same time. A's text lands in the tombstone; A is told
/// who deleted it, with Restore; Restore brings it back on both peers with A's text.
#[test]
fn typing_into_a_deleted_shape_offers_restore() {
    for seed in 1..5 {
        let mut r = Room::new(basic(), 2, seed);
        let a = find(&r.peers[0].doc(), "alpha");
        r.peers[0].run("text.edit", json!({"id": a.0})).expect("edit");
        r.peers[0].run("text.insert", json!({"text": "Z"})).expect("type");
        r.peers[1].run("edit.delete", json!({"ids": [a.0]})).expect("delete");
        r.settle();
        for i in 0..2 {
            assert!(text_of(&r, i, a).is_none(), "peer {i}: the delete wins (seed {seed})");
        }
        r.assert_converged();
        let offer = r.peers[0].notices.iter().find(|n| n.restore == Some(Tomb::Shape(a))).cloned().expect("A is offered Restore");
        assert_eq!(offer.text, deleted_text("User 1"), "the notice names the deleter by display name");
        assert!(r.peers[1].notices.iter().all(|n| n.restore.is_none()), "the deleter is not offered its own delete");

        let p = &mut r.peers[0];
        p.live.restore(&mut p.session, Tomb::Shape(a)).expect("restore");
        r.settle();
        for i in 0..2 {
            let t = text_of(&r, i, a).expect("the shape is back");
            assert!(t.contains('Z') && t.contains("alpha"), "peer {i}: back with A's text: {t:?} (seed {seed})");
        }
        r.assert_converged();
    }
}

/// Restoring a shape whose slide was deleted too brings the slide back with it.
#[test]
fn restoring_a_shape_on_a_deleted_slide_brings_the_slide_back() {
    let mut r = Room::new(basic(), 2, 3);
    let a = find(&r.peers[0].doc(), "alpha");
    let slide = r.peers[0].doc().slides[0].id;
    r.peers[0].run("shape.move", json!({"ids": [a.0], "dx": 30.0})).expect("move");
    r.peers[1].run("edit.delete", json!({"ids": [a.0]})).expect("delete shape");
    r.settle();
    r.peers[1].run("slide.delete", json!({"index": 0})).expect("delete slide");
    r.settle();
    assert!(r.peers[0].slide_ids().iter().all(|s| *s != slide), "the slide is gone");
    let p = &mut r.peers[0];
    p.live.restore(&mut p.session, Tomb::Shape(a)).expect("restore");
    r.settle();
    for i in 0..2 {
        assert!(r.peers[i].slide_ids().contains(&slide), "peer {i}: the slide is back");
        assert!(text_of(&r, i, a).is_some(), "peer {i}: the shape is back");
    }
    r.assert_converged();
}

/// A deleted slide that holds my selection: told, with Restore, naming the slide.
#[test]
fn deleting_the_slide_i_work_on_offers_restore() {
    let mut r = Room::new(basic(), 2, 4);
    let a = find(&r.peers[0].doc(), "alpha");
    let slide = r.peers[0].doc().slides[0].id;
    r.peers[0].run("edit.select", json!({"ids": [a.0]})).expect("select");
    r.peers[1].run("slide.delete", json!({"index": 0})).expect("delete slide");
    r.settle();
    let offer = r.peers[0].notices.iter().find(|n| n.restore == Some(Tomb::Slide(slide))).cloned().expect("offered Restore");
    assert_eq!(offer.text, deleted_slide_text("User 1"));
    r.assert_converged();
}

/// A drop on a shape a peer deleted during the drag is refused with the deleter's name, and
/// nothing of the drag is shared.
#[test]
fn a_drop_on_a_deleted_shape_is_refused_with_the_name() {
    let mut r = Room::new(basic(), 2, 5);
    let a = find(&r.peers[0].doc(), "alpha");
    r.peers[0].live.take_sent();
    // alpha is [60, 60, 200, 80]: press inside it and drag.
    r.peers[0].session.pointer(ev(PointerKind::Down, 160.0, 100.0)).expect("down");
    r.peers[0].session.pointer(ev(PointerKind::Drag, 200.0, 120.0)).expect("drag");
    r.peers[1].run("edit.delete", json!({"ids": [a.0]})).expect("delete");
    for _ in 0..4 {
        r.pump_all();
    }
    assert!(r.peers[0].live.holding(), "the delete waits for the drop");
    r.peers[0].session.pointer(ev(PointerKind::Up, 220.0, 130.0)).expect("up");
    r.settle();
    // The refusal is a line of its own, without Restore (the delete notice, if any, has it).
    assert!(
        r.peers[0].notices.iter().any(|n| n.text == deleted_text("User 1") && n.restore.is_none()),
        "the drop is refused with the name: {:?}",
        r.peers[0].notices
    );
    assert!(r.peers[0].live.take_sent().iter().all(|g| !g.shapes.contains(&a)), "nothing of the drag went out");
    for i in 0..2 {
        assert!(text_of(&r, i, a).is_none(), "peer {i}: the shape stays deleted");
    }
    r.assert_converged();
}

/// Every connector end and animation on `slide` that names `id`.
fn mentions(p: &Presentation, slide: SlideId, id: ShapeId) -> usize {
    let Some(sl) = p.slides.iter().find(|s| s.id == slide) else { return 0 };
    let mut n = sl.animations.iter().filter(|a| a.shape == id || a.trigger == Some(id)).count();
    deckcraft_model::walk(&sl.shapes, &mut |sh, _| {
        if let ShapeKind::Connector { start, end } = &sh.kind {
            n += [start, end].iter().filter(|e| e.is_some_and(|(to, _)| to == id)).count();
        }
    });
    n
}

/// The deleted shape is a connector end, animated, and the trigger of another animation: every
/// peer, and the deck rebuilt from the state, equal what a local `edit.delete` makes.
#[test]
fn remote_delete_cleans_like_a_local_delete() {
    let bytes = deck_bytes(|s| {
        add_box(s, "target", [60.0, 60.0, 200.0, 80.0]);
        add_box(s, "other", [400.0, 260.0, 200.0, 80.0]);
        let doc = s.active().expect("doc").doc.clone();
        let (t, o) = (find(&doc, "target"), find(&doc, "other"));
        s.execute("shape.connect", &json!({"from": t.0, "to": o.0})).expect("connect");
        s.execute("animation.add", &json!({"effect": "fade", "ids": [t.0]})).expect("animate");
        s.execute("animation.add", &json!({"effect": "spin", "ids": [o.0]})).expect("animate other");
        s.execute("animation.timing", &json!({"index": 1, "trigger": t.0})).expect("triggered");
        s.execute("edit.deselect", &json!({})).expect("deselect");
    });
    let mut local = Session::new();
    let deck = deckcraft_engine::cmd::file::open_presentation(NAME, &bytes).expect("open");
    deckcraft_engine::cmd::file::add_opened(&mut local, NAME, None, deck);
    let t = find(&local.active().expect("doc").doc, "target");
    let slide = local.active().expect("doc").doc.slides[0].id;
    assert!(mentions(&local.active().expect("doc").doc, slide, t) >= 3, "a connector end, animated and a trigger");
    local.execute("edit.delete", &json!({"ids": [t.0]})).expect("local delete");
    let want = local.active().expect("doc").doc.clone();
    assert_eq!(mentions(&want, slide, t), 0);

    let mut r = Room::new(bytes, 3, 6);
    r.peers[1].run("edit.delete", json!({"ids": [t.0]})).expect("delete");
    r.settle();
    for (i, p) in r.peers.iter().enumerate() {
        assert_eq!(p.doc().slides, want.slides, "peer {i} equals the local delete");
        let state = p.live.replica().expect("replica").state();
        assert_eq!(deck_from_state(p.live.base(), &state).expect("rebuild").slides, want.slides, "peer {i}: the state rebuilds to it");
    }
    r.assert_converged();
}

/// A peer's junk in `del` must not revive a deleted shape: `delBy` still names the deleter.
#[test]
fn junk_del_on_a_tombstone_keeps_it_deleted() {
    let bytes = basic();
    let deck = Arc::new(deckcraft_engine::cmd::file::open_presentation(NAME, &bytes).expect("open"));
    let a = find(&deck, "alpha");
    let seeded = DeckDoc::seeded(&DeckSeed::new(&bytes, deck.clone())).expect("seed");
    let peer = |space: u32| {
        let mut d = DeckDoc::from_state(&seeded.state()).expect("state");
        d.set_client(space).expect("space");
        d
    };
    let (mut x, mut y, mut z) = (peer(3), peer(4), peer(5));
    let (s1, s2) = (deck.slides[1].id, deck.slides[2].id);
    let b = find(&deck, "beta");
    let del = x
        .write(false, |t| {
            for target in [Tomb::Shape(a), Tomb::Shape(b), Tomb::Slide(s1), Tomb::Slide(s2)] {
                t.set_del(target, true, "user1");
            }
            Ok(())
        })
        .expect("delete");
    y.apply(&del).expect("apply");
    z.apply(&del).expect("apply");
    let junk = z
        .write(false, |t| {
            t.set_shape(a, "del", json!("maybe"));
            t.set_slide(s1, "del", json!({}));
            Ok(())
        })
        .expect("junk");
    y.apply(&junk).expect("apply");
    // A removed `del` next to the deleter's name: the same, through raw yrs.
    y.apply(&remove_del(&z, &[("shapes", b.0), ("slides", s2.0)])).expect("apply");
    let live = deck_from_doc(&deck, &y);
    for id in [a, b] {
        assert!(shape_of(&live, id).is_none(), "the tombstone stays");
        assert!(y.shapes_of(deck.slides[0].id).iter().all(|x| *x != id), "the index reads it deleted too");
    }
    for s in [s1, s2] {
        assert!(live.slides.iter().all(|x| x.id != s), "the slide stays deleted");
        assert!(!y.slide_order().contains(&s), "the index reads the slide deleted too");
    }
    assert_eq!(live, deck_from_state(&deck, &y.state()).expect("rebuild"), "live and rebuilt agree");
}

/// An update from a peer of `doc`'s state that removes the `del` key of each (root, id) entry.
fn remove_del(doc: &DeckDoc, entries: &[(&str, u32)]) -> Vec<u8> {
    use craft_collab::yrs::updates::decoder::Decode;
    use craft_collab::yrs::{Doc, Map, Out, ReadTxn, Transact, Update};
    let raw = Doc::with_client_id(77);
    raw.transact_mut().apply_update(Update::decode_v1(&doc.state()).expect("state")).expect("apply");
    let sv = raw.transact().state_vector();
    for (root, id) in entries {
        let map = raw.get_or_insert_map(*root);
        let mut t = raw.transact_mut();
        if let Some(Out::YMap(m)) = map.get(&t, &deckcraft_live::schema::id_key(*id)) {
            m.remove(&mut t, "del");
        }
    }
    raw.transact().encode_state_as_update_v1(&sv)
}

/// The relay grants a space past the id block range: editing goes on, and a minting command
/// says why it is refused instead of waiting for a connection that is already there.
#[test]
fn a_space_past_the_block_range_still_edits() {
    let bytes = basic();
    let mut r = Room::new(bytes.clone(), 1, 8);
    // Peer 0 keeps a space near the top of the counter: the relay floors the counter there.
    r.peers[0].live.core_mut().hub_mut().set_space(deckcraft_model::IdBlock::SPACES + 10);
    r.drop_link(0);
    r.restore_link(0);
    r.settle();
    let i = r.join(&bytes);
    let a = find(&r.peers[i].doc(), "alpha");
    let shapes = |r: &Room, i: usize| r.peers[i].doc().slides[0].shapes.len();
    let n = shapes(&r, i);
    let refused = r.peers[i].run("shape.insert", json!({"preset": "rect"})).expect_err("no block to mint from");
    assert_eq!(refused, deckcraft_live::NO_SPACE_TEXT);
    // Honest (rule 15): a reopen gets a space from the same counter, so the text never offers it.
    let lower = refused.to_lowercase();
    assert!(!lower.contains("reopen") && lower.contains("copy"), "{refused}");
    assert_eq!(shapes(&r, i), n, "nothing was added");
    r.peers[i].run("shape.move", json!({"ids": [a.0], "dx": 40.0})).expect("a move is still shared");
    r.settle();
    assert!(
        r.peers[0].doc().slides[0].shape(a).and_then(|s| s.xfrm) != shape_of(&deck_of(&bytes), a).and_then(|s| s.xfrm),
        "the move reached the room"
    );
    r.assert_converged();
}

fn deck_of(bytes: &[u8]) -> Presentation {
    deckcraft_engine::cmd::file::open_presentation(NAME, bytes).expect("open")
}

/// With this peer's id block used up, a new slide is refused with the reason to reopen.
#[test]
fn a_used_up_id_block_says_reopen() {
    let mut r = Room::new(basic(), 2, 9);
    // The block's counter is shared by every clone of the deck (Ruling D5).
    let mut p: Presentation = (*r.peers[0].doc()).clone();
    let mut n = 0u32;
    while p.alloc_id().is_some() {
        n += 1;
        assert!(n <= deckcraft_model::IdBlock::PER_SPACE, "the block ends");
    }
    let (slides, seq) = (r.peers[0].slide_ids().len(), r.relay.seq());
    let refused = r.peers[0].run("slide.new", json!({"layout": "titleOnly"})).expect_err("refused");
    assert_eq!(refused, deckcraft_live::NO_SPACE_TEXT);
    r.settle();
    assert_eq!(r.peers[0].slide_ids().len(), slides, "no slide was added");
    assert_eq!(r.relay.seq(), seq, "nothing was sent");
    r.assert_converged();
}

/// Restore takes back the delete's own cleanup too: the connector glued to the deleted shape is
/// glued again, as in the base, on every peer.
#[test]
fn restore_reattaches_the_connector() {
    let bytes = deck_bytes(|s| {
        add_box(s, "target", [60.0, 60.0, 200.0, 80.0]);
        add_box(s, "other", [400.0, 260.0, 200.0, 80.0]);
        let doc = s.active().expect("doc").doc.clone();
        let (t, o) = (find(&doc, "target"), find(&doc, "other"));
        s.execute("shape.connect", &json!({"from": t.0, "to": o.0})).expect("connect");
        s.execute("edit.deselect", &json!({})).expect("deselect");
    });
    let base = deck_of(&bytes);
    let t = find(&base, "target");
    let slide = base.slides[0].id;
    assert!(mentions(&base, slide, t) >= 1, "the connector is glued to the target");
    let mut r = Room::new(bytes, 2, 11);
    r.peers[1].run("edit.delete", json!({"ids": [t.0]})).expect("delete");
    r.settle();
    assert_eq!(mentions(&r.peers[0].doc(), slide, t), 0, "the delete detached it");
    let p = &mut r.peers[0];
    p.live.restore(&mut p.session, Tomb::Shape(t)).expect("restore");
    r.settle();
    for (i, p) in r.peers.iter().enumerate() {
        assert_eq!(p.doc().slides[0], base.slides[0], "peer {i}: the slide is the base's again, connector glued");
    }
    r.assert_converged();
}

/// My last 20 groups count without a selection; the 21st back does not.
#[test]
fn my_last_twenty_groups_count_without_a_selection() {
    let mut r = Room::new(basic(), 2, 12);
    let doc = r.peers[0].doc();
    let (a, b, g) = (find(&doc, "alpha"), find(&doc, "beta"), find(&doc, "gamma"));
    r.peers[0].run("shape.move", json!({"ids": [a.0], "dx": 5.0})).expect("move");
    r.peers[0].run("shape.move", json!({"ids": [b.0], "dx": 5.0})).expect("move");
    for _ in 0..19 {
        r.peers[0].run("shape.move", json!({"ids": [g.0], "dx": 1.0})).expect("move");
    }
    r.peers[0].run("edit.deselect", json!({})).expect("deselect");
    r.settle();
    r.peers[1].run("edit.delete", json!({"ids": [a.0, b.0]})).expect("delete");
    r.settle();
    let offered = |t| r.peers[0].notices.iter().any(|n| n.restore == Some(t));
    assert!(offered(Tomb::Shape(b)), "beta is in my last 20 groups");
    assert!(!offered(Tomb::Shape(a)), "alpha is 21 groups back");
    r.assert_converged();
}
