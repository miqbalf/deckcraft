//! Shared edits between peers: each lands on every other peer, and every peer ends showing what
//! its replica describes (L4).

use std::collections::BTreeSet;

use deckcraft_engine::PointerKind;
use deckcraft_live::EDIT_TOO_LARGE_TEXT;
use deckcraft_model::{ShapeId, SlideId};
use serde_json::json;

use crate::support::{Room, add_box, basic, deck_bytes, ev, find, shape_of, space_of};

fn x_of(r: &Room, peer: usize, id: ShapeId) -> f64 {
    shape_of(&r.peers[peer].doc(), id).and_then(|s| s.xfrm).map(|x| x.x).expect("placed shape")
}

fn text_of(r: &Room, peer: usize, id: ShapeId) -> String {
    shape_of(&r.peers[peer].doc(), id).and_then(|s| s.text).map(|t| t.text()).unwrap_or_default()
}

#[test]
fn typing_reaches_the_other_peer_and_equals_the_rebuild() {
    let mut r = Room::new(basic(), 2, 1);
    let a = find(&r.peers[0].doc(), "alpha");
    r.peers[0].run("text.edit", json!({"id": a.0})).expect("edit");
    r.peers[0].run("text.insert", json!({"text": "Z"})).expect("type");
    r.settle();
    let seen = text_of(&r, 1, a);
    assert!(seen.contains('Z') && seen.contains("alpha"), "the other peer sees the typing: {seen:?}");
    r.assert_converged();
}

#[test]
fn move_and_recolour_of_one_shape_keep_both() {
    let mut r = Room::new(basic(), 2, 2);
    let a = find(&r.peers[0].doc(), "alpha");
    let x0 = x_of(&r, 0, a);
    let fill0 = shape_of(&r.peers[0].doc(), a).and_then(|s| s.fill);
    r.peers[0].run("shape.move", json!({"ids": [a.0], "dx": 50.0})).expect("move");
    r.peers[1].run("shape.fill", json!({"ids": [a.0], "color": "#C00000"})).expect("fill");
    r.settle();
    for i in 0..2 {
        assert!(x_of(&r, i, a) > x0, "peer {i} keeps the move");
        assert!(shape_of(&r.peers[i].doc(), a).and_then(|s| s.fill) != fill0, "peer {i} keeps the colour");
    }
    r.assert_converged();
}

#[test]
fn two_moves_of_one_shape_end_equal_everywhere() {
    for seed in 1..5 {
        let mut r = Room::new(basic(), 3, seed);
        let a = find(&r.peers[0].doc(), "alpha");
        let x0 = x_of(&r, 0, a);
        r.peers[0].run("shape.move", json!({"ids": [a.0], "dx": 50.0})).expect("move");
        r.peers[2].run("shape.move", json!({"ids": [a.0], "dx": -30.0})).expect("move");
        r.settle();
        assert!(x_of(&r, 1, a) != x0, "one of the moves won (seed {seed})");
        r.assert_converged();
    }
}

#[test]
fn concurrent_slide_reorders_never_duplicate() {
    for seed in 1..7 {
        let mut r = Room::new(basic(), 3, seed);
        let before: BTreeSet<SlideId> = r.peers[0].slide_ids().into_iter().collect();
        r.peers[0].run("slide.move", json!({"from": 0, "to": 2})).expect("move");
        r.peers[1].run("slide.move", json!({"from": 2, "to": 0})).expect("move");
        r.peers[2].run("slide.move", json!({"from": 1, "to": 0})).expect("move");
        r.settle();
        for (i, p) in r.peers.iter().enumerate() {
            let ids = p.slide_ids();
            let set: BTreeSet<SlideId> = ids.iter().copied().collect();
            assert!(ids.len() == set.len() && set == before, "peer {i} has each slide once (seed {seed})");
        }
        r.assert_converged();
    }
}

#[test]
fn new_slide_from_a_layout_gets_ids_from_my_block() {
    let mut r = Room::new(basic(), 2, 3);
    let before: BTreeSet<SlideId> = r.peers[0].slide_ids().into_iter().collect();
    let mine = r.peers[1].space();
    assert!(mine != 0 && mine != r.peers[0].space(), "each peer has a space of its own");
    r.peers[1].run("slide.new", json!({"layout": "titleAndContent", "title": "Four"})).expect("new slide");
    r.settle();
    let doc = r.peers[0].doc();
    let added: Vec<_> = doc.slides.iter().filter(|s| !before.contains(&s.id)).collect();
    assert!(added.len() == 1, "the other peer sees one new slide");
    let s = added[0];
    assert!(space_of(s.id.0) == mine, "the slide id comes from the author's block");
    assert!(!s.shapes.is_empty(), "the layout's placeholders came along");
    assert!(s.shapes.iter().all(|sh| space_of(sh.id.0) == mine), "every shape id comes from the author's block");
    r.assert_converged();
}

#[test]
fn duplicate_slide_and_paste_shapes_are_shared() {
    let mut r = Room::new(basic(), 2, 4);
    let a = find(&r.peers[0].doc(), "alpha");
    let n = r.peers[0].doc().slides.len();
    let on_first = r.peers[0].doc().slides[0].shapes.len();
    r.peers[0].run("slide.duplicate", json!({"index": 1})).expect("duplicate");
    r.peers[1].run("edit.select", json!({"ids": [a.0]})).expect("select");
    r.peers[1].run("edit.copy", json!({})).expect("copy");
    r.peers[1].run("edit.paste", json!({})).expect("paste");
    r.settle();
    let doc = r.peers[0].doc();
    assert!(doc.slides.len() == n + 1, "the copy of the slide reached the other peer");
    let first = doc.slides.iter().find(|s| s.shape(a).is_some()).expect("the first slide");
    assert!(first.shapes.len() > on_first, "the pasted shape reached the other peer");
    let alphas = first.shapes.iter().filter(|s| s.text.as_ref().is_some_and(|t| t.text() == "alpha")).count();
    assert!(alphas > 1, "the pasted shape carries the copied text");
    r.assert_converged();
}

#[test]
fn held_remote_change_lands_after_the_drop() {
    let mut r = Room::new(basic(), 2, 5);
    let doc = r.peers[0].doc();
    let (a, b) = (find(&doc, "alpha"), find(&doc, "beta"));
    let x0 = x_of(&r, 0, a);
    let fill_b = shape_of(&doc, b).and_then(|s| s.fill);
    r.peers[0].live.take_sent();
    // alpha is [60, 60, 200, 80]: press inside it and drag.
    r.peers[0].session.pointer(ev(PointerKind::Down, 160.0, 100.0)).expect("down");
    r.peers[0].session.pointer(ev(PointerKind::Drag, 200.0, 120.0)).expect("drag");
    r.peers[1].run("shape.fill", json!({"ids": [b.0], "color": "#00A000"})).expect("fill");
    for _ in 0..4 {
        r.pump_all();
    }
    assert!(r.peers[0].live.holding(), "the remote change waits for the drop");
    assert!(shape_of(&r.peers[0].doc(), b).and_then(|s| s.fill) == fill_b, "nothing landed under the drag");
    r.peers[0].session.pointer(ev(PointerKind::Up, 220.0, 130.0)).expect("up");
    r.settle();
    assert!(x_of(&r, 0, a) > x0, "the drag landed");
    assert!(shape_of(&r.peers[0].doc(), b).and_then(|s| s.fill) != fill_b, "the held change landed after the drop");
    let sent = r.peers[0].live.take_sent();
    assert!(!sent.is_empty(), "the drag was shared");
    assert!(sent.iter().all(|g| g.shapes.contains(&a) && !g.shapes.contains(&b)), "the drag's group holds only its own shape");
    r.assert_converged();
}

/// Esc cancels a gesture by putting back the document from before it. In the engine only an IME
/// composition and an Edit Points drag can be cancelled (a move drag has no cancel), and Edit
/// Points is not shared yet, so the cancelled gesture here is a composition: its empty preedit is
/// what Esc sends.
#[test]
fn cancelled_drag_keeps_the_remote_change() {
    let mut r = Room::new(basic(), 2, 6);
    let a = find(&r.peers[0].doc(), "alpha");
    let fill0 = shape_of(&r.peers[0].doc(), a).and_then(|s| s.fill);
    r.peers[0].run("text.edit", json!({"id": a.0})).expect("edit");
    r.peers[0].run("text.compose", json!({"text": "k"})).expect("compose");
    r.peers[1].run("shape.fill", json!({"ids": [a.0], "color": "#0000C0"})).expect("fill");
    for _ in 0..4 {
        r.pump_all();
    }
    assert!(r.peers[0].live.holding(), "the remote change waits for the composition");
    r.peers[0].session.compose_text("", None).expect("cancel");
    r.settle();
    assert!(text_of(&r, 0, a) == "alpha", "the cancelled composition left no text");
    assert!(shape_of(&r.peers[0].doc(), a).and_then(|s| s.fill) != fill0, "the remote colour stays");
    r.assert_converged();
}

#[test]
fn two_commands_between_pumps_are_two_groups() {
    let mut r = Room::new(basic(), 2, 7);
    let doc = r.peers[0].doc();
    let (a, b) = (find(&doc, "alpha"), find(&doc, "beta"));
    r.peers[0].live.take_sent();
    r.peers[0].run("shape.fill", json!({"ids": [a.0], "color": "#C00000"})).expect("fill");
    r.peers[0].run("shape.fill", json!({"ids": [b.0], "color": "#00C000"})).expect("fill");
    let sent = r.peers[0].live.take_sent();
    assert!(sent.len() == 2, "one group per command");
    assert!(sent[0].shapes.contains(&a) && !sent[0].shapes.contains(&b), "the first group is the first command");
    assert!(sent[1].shapes.contains(&b) && !sent[1].shapes.contains(&a), "the second group is the second command");
    r.settle();
    r.assert_converged();
}

/// A slide of three boxes, each with text near half the update budget.
fn heavy() -> Vec<u8> {
    let long = "lorem ipsum ".repeat(30_000);
    deck_bytes(|s| {
        for i in 0..3 {
            add_box(s, &format!("{i}{long}"), [40.0 + 200.0 * f64::from(i), 100.0, 180.0, 120.0]);
        }
    })
}

#[test]
fn a_command_over_the_budget_is_sent_in_parts_and_lands_whole() {
    let mut r = Room::new(heavy(), 2, 8);
    let n = r.peers[1].doc().slides.len();
    r.peers[0].live.take_sent();
    r.peers[0].run("slide.duplicate", json!({"index": 0})).expect("duplicate");
    let sent = r.peers[0].live.take_sent();
    assert!(sent.len() == 1 && sent[0].updates > 1, "one command, several updates");
    r.settle();
    assert!(r.peers[1].doc().slides.len() == n + 1, "the copy landed whole");
    r.assert_converged();
}

#[test]
fn an_edit_too_large_for_the_link_is_refused() {
    let mut r = Room::new(basic(), 2, 9);
    let a = find(&r.peers[0].doc(), "alpha");
    let before = r.peers[0].doc();
    let seq = r.relay.seq();
    let huge = "x".repeat(3 * 1024 * 1024);
    let got = r.peers[0].run("text.set", json!({"id": a.0, "text": huge}));
    assert!(got == Err(EDIT_TOO_LARGE_TEXT.to_owned()), "the edit is refused with the reason");
    assert!(*r.peers[0].doc() == *before, "the deck is put back");
    assert!(r.relay.seq() == seq, "nothing was sent");
    r.settle();
    r.assert_converged();
}

#[test]
fn typing_into_a_shape_with_no_text_is_shared() {
    let bytes = deck_bytes(|s| {
        s.execute("shape.insert", &json!({"preset": "line", "rect": [40.0, 40.0, 200.0, 0.0]})).expect("line");
    });
    let mut r = Room::new(bytes, 2, 10);
    let line = r.peers[0].doc().slides[0].shapes.iter().find(|s| s.text.is_none()).map(|s| s.id).expect("a shape with no text");
    r.peers[0].run("text.set", json!({"id": line.0, "text": "label"})).expect("set");
    r.settle();
    assert!(text_of(&r, 1, line) == "label", "the new text body reached the other peer");
    r.assert_converged();
}

#[test]
fn a_composition_commits_onto_held_remote_text() {
    let mut r = Room::new(basic(), 2, 11);
    let a = find(&r.peers[0].doc(), "alpha");
    r.peers[0].run("text.edit", json!({"id": a.0})).expect("edit");
    r.peers[0].run("text.compose", json!({"text": "k"})).expect("compose");
    r.peers[1].run("text.edit", json!({"id": a.0})).expect("edit");
    r.peers[1].run("text.insert", json!({"text": "Q"})).expect("type");
    for _ in 0..4 {
        r.pump_all();
    }
    assert!(r.peers[0].live.holding(), "the remote typing waits for the composition");
    r.peers[0].run("text.commit", json!({"text": "ki"})).expect("commit");
    r.settle();
    for i in 0..2 {
        let t = text_of(&r, i, a);
        assert!(t.contains("ki") && t.contains('Q') && t.contains("alpha"), "peer {i} keeps both: {t:?}");
    }
    r.assert_converged();
}
