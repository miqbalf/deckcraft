//! Per-user undo and redo (L6, Ruling D4): one Ctrl+Z undoes one command of mine, however many
//! updates carried it; text by its own characters, properties only where nobody changed them since.

use deckcraft_live::{AFTER_UNDO_TEXT, DeckDoc, Tomb, UNDO_SKIPPED_TEXT};
use deckcraft_model::{Presentation, ShapeId, SlideId};
use serde_json::json;

use crate::support::{Room, add_box, basic, deck_bytes, find, shape_of};

fn text_of(r: &Room, peer: usize, id: ShapeId) -> String {
    shape_of(&r.peers[peer].doc(), id).and_then(|s| s.text).map(|t| t.text()).unwrap_or_default()
}

fn fill_of(r: &Room, peer: usize, id: ShapeId) -> Option<deckcraft_model::Fill> {
    shape_of(&r.peers[peer].doc(), id).and_then(|s| s.fill)
}

fn x_of(r: &Room, peer: usize, id: ShapeId) -> f64 {
    shape_of(&r.peers[peer].doc(), id).and_then(|s| s.xfrm).map(|x| x.x).expect("placed shape")
}

fn undo(r: &mut Room, peer: usize) {
    r.peers[peer].run("edit.undo", json!({})).expect("undo");
}

fn redo(r: &mut Room, peer: usize) {
    r.peers[peer].run("edit.redo", json!({})).expect("redo");
}

#[test]
fn undo_reverts_only_my_text() {
    for seed in 1..=3 {
        let mut r = Room::new(basic(), 2, seed);
        let a = find(&r.peers[0].doc(), "alpha");
        r.peers[0].run("text.edit", json!({"id": a.0})).expect("edit");
        r.peers[0].run("text.insert", json!({"text": "X"})).expect("type");
        r.peers[1].run("text.edit", json!({"id": a.0})).expect("edit");
        r.peers[1].run("text.insert", json!({"text": "Q"})).expect("type");
        r.settle();
        let both = text_of(&r, 0, a);
        assert!(both.contains('X') && both.contains('Q'), "both typists' characters are there: {both:?}");
        undo(&mut r, 0);
        r.settle();
        for i in 0..2 {
            let t = text_of(&r, i, a);
            assert!(!t.contains('X'), "peer {i}: my character is undone: {t:?}");
            assert!(t.contains('Q') && t.contains("alpha"), "peer {i}: the other typist's character stays: {t:?}");
        }
        r.assert_converged();
    }
}

#[test]
fn undo_after_a_peer_edit_leaves_the_peer_edit() {
    let mut r = Room::new(basic(), 2, 2);
    let a = find(&r.peers[0].doc(), "alpha");
    let fill0 = fill_of(&r, 0, a);
    r.peers[0].run("shape.fill", json!({"ids": [a.0], "color": "#C00000"})).expect("fill");
    r.settle();
    r.peers[1].run("shape.move", json!({"ids": [a.0], "dx": 50.0})).expect("move");
    r.settle();
    let moved = x_of(&r, 1, a);
    undo(&mut r, 0);
    r.settle();
    for i in 0..2 {
        assert!(fill_of(&r, i, a) == fill0, "peer {i}: my colour is undone");
        assert!(x_of(&r, i, a) == moved, "peer {i}: the other editor's move stays");
    }
    r.assert_converged();
}

#[test]
fn undo_skips_a_key_someone_changed_since_and_says_so() {
    let mut r = Room::new(basic(), 2, 3);
    let a = find(&r.peers[0].doc(), "alpha");
    r.peers[0].run("shape.fill", json!({"ids": [a.0], "color": "#C00000"})).expect("fill");
    r.settle();
    r.peers[1].run("shape.fill", json!({"ids": [a.0], "color": "#00A000"})).expect("fill");
    r.settle();
    let theirs = fill_of(&r, 1, a);
    r.peers[0].notices.clear();
    undo(&mut r, 0);
    r.settle();
    for i in 0..2 {
        assert!(fill_of(&r, i, a) == theirs, "peer {i}: the later colour stays");
    }
    assert!(r.peers[0].notices.iter().any(|n| n.text == UNDO_SKIPPED_TEXT), "I am told what was not undone");
    assert!(!r.peers[0].live.can_redo(), "an undo that did nothing offers no redo");
    r.assert_converged();
}

/// One slide of 2,000 small boxes.
fn crowded() -> Vec<u8> {
    deck_bytes(|s| {
        for i in 0..2000u32 {
            let (x, y) = (f64::from(i % 50) * 18.0, f64::from(i / 50) * 12.0);
            add_box(s, "", [x, y, 16.0, 10.0]);
        }
    })
}

#[test]
fn one_command_is_one_undo_step_across_split_updates() {
    let mut r = Room::new(crowded(), 2, 4);
    let before: Vec<Presentation> = r.peers.iter().map(|p| (*p.doc()).clone()).collect();
    r.peers[0].run("edit.selectAll", json!({})).expect("select all");
    r.peers[0].run("edit.copy", json!({})).expect("copy");
    r.peers[0].live.take_sent();
    r.peers[0].run("edit.paste", json!({})).expect("paste");
    let sent = r.peers[0].live.take_sent();
    assert!(sent.len() == 1 && sent[0].updates > 1, "one paste, several updates");
    r.settle();
    assert!(r.peers[1].doc().slides[0].shapes.len() > before[1].slides[0].shapes.len(), "the paste reached the other peer");
    undo(&mut r, 0);
    let sent = r.peers[0].live.take_sent();
    assert!(sent.len() == 1, "one undo, one group");
    r.settle();
    for (i, b) in before.iter().enumerate() {
        assert!(r.peers[i].doc().slides == b.slides, "peer {i}: one Ctrl+Z took the whole paste away");
    }
    r.assert_converged();
}

fn slide_entry_dead(doc: &DeckDoc, id: SlideId) -> Option<bool> {
    doc.slide(id).map(|e| e.del)
}

#[test]
fn undo_of_a_new_slide_tombstones_it_everywhere() {
    let mut r = Room::new(basic(), 2, 5);
    let ids0 = r.peers[0].slide_ids();
    r.peers[0].run("slide.new", json!({"layout": "titleOnly", "title": "Four"})).expect("new slide");
    r.settle();
    let new: Vec<SlideId> = r.peers[1].slide_ids().into_iter().filter(|id| !ids0.contains(id)).collect();
    assert!(new.len() == 1, "the new slide reached the other peer");
    undo(&mut r, 0);
    r.settle();
    for (i, p) in r.peers.iter().enumerate() {
        assert!(p.slide_ids() == ids0, "peer {i}: the new slide is gone");
        let replica = p.live.replica().expect("replica");
        assert!(slide_entry_dead(replica, new[0]) == Some(true), "peer {i}: its entry is a tombstone, not removed");
    }
    r.assert_converged();
}

#[test]
fn undo_of_my_delete_restores_it_everywhere() {
    let mut r = Room::new(basic(), 2, 6);
    let a = find(&r.peers[0].doc(), "alpha");
    let was = shape_of(&r.peers[0].doc(), a);
    let deck0 = (*r.peers[1].doc()).clone();
    r.peers[0].run("edit.delete", json!({"ids": [a.0]})).expect("delete");
    r.settle();
    assert!(shape_of(&r.peers[1].doc(), a).is_none(), "the delete reached the other peer");
    undo(&mut r, 0);
    r.settle();
    for (i, p) in r.peers.iter().enumerate() {
        assert!(shape_of(&p.doc(), a) == was, "peer {i}: the shape is back as it was");
        assert!(p.doc().slides == deck0.slides, "peer {i}: the deck is as before the delete");
    }
    r.assert_converged();
}

#[test]
fn redo_mirrors_undo() {
    let mut r = Room::new(basic(), 2, 7);
    let a = find(&r.peers[0].doc(), "alpha");
    r.peers[0].run("shape.fill", json!({"ids": [a.0], "color": "#C00000"})).expect("fill");
    r.peers[0].run("text.edit", json!({"id": a.0})).expect("edit");
    r.peers[0].run("text.insert", json!({"text": "Z"})).expect("type");
    r.settle();
    let done = (*r.peers[1].doc()).clone();
    undo(&mut r, 0);
    undo(&mut r, 0);
    r.settle();
    assert!(!text_of(&r, 1, a).contains('Z'), "the typing is undone");
    assert!(*r.peers[1].doc() != done, "the colour is undone");
    assert!(r.peers[0].live.can_redo(), "there is something to redo");
    redo(&mut r, 0);
    redo(&mut r, 0);
    r.settle();
    for (i, p) in r.peers.iter().enumerate() {
        assert!(p.doc().slides == done.slides, "peer {i}: redo brings both back");
    }
    assert!(!r.peers[0].live.can_redo(), "nothing more to redo");
    r.assert_converged();
}

#[test]
fn a_new_command_clears_redo() {
    let mut r = Room::new(basic(), 2, 8);
    let a = find(&r.peers[0].doc(), "alpha");
    let fill0 = fill_of(&r, 0, a);
    r.peers[0].run("shape.fill", json!({"ids": [a.0], "color": "#C00000"})).expect("fill");
    undo(&mut r, 0);
    assert!(r.peers[0].live.can_redo(), "the colour can be redone");
    r.peers[0].run("shape.move", json!({"ids": [a.0], "dx": 20.0})).expect("move");
    assert!(!r.peers[0].live.can_redo(), "a new command clears redo");
    redo(&mut r, 0);
    r.settle();
    for i in 0..2 {
        assert!(fill_of(&r, i, a) == fill0, "peer {i}: redo did nothing");
    }
    r.assert_converged();
}

#[test]
fn n_undos_then_n_redos_leave_the_deck_unchanged() {
    let mut r = Room::new(basic(), 2, 9);
    let a = find(&r.peers[0].doc(), "alpha");
    let b = find(&r.peers[0].doc(), "beta");
    let steps: Vec<(&str, serde_json::Value)> = vec![
        ("text.edit", json!({"id": a.0})),
        ("text.insert", json!({"text": "Hello"})),
        ("shape.fill", json!({"ids": [a.0], "color": "#C00000"})),
        ("shape.move", json!({"ids": [b.0], "dx": 40.0})),
        ("slide.new", json!({"layout": "titleOnly", "title": "Four"})),
        ("slide.move", json!({"from": 0, "to": 2})),
        ("slide.go", json!({"index": 1})),
        ("edit.delete", json!({"ids": [b.0]})),
    ];
    for (id, p) in &steps {
        r.peers[0].run(id, p.clone()).expect(id);
    }
    r.settle();
    let done = (*r.peers[0].doc()).clone();
    let mut n = 0;
    while r.peers[0].live.can_undo() {
        undo(&mut r, 0);
        n += 1;
        assert!(n < 100, "the undo log ends");
    }
    r.settle();
    assert!(*r.peers[0].doc() != done, "the undos changed the deck");
    for _ in 0..n {
        redo(&mut r, 0);
    }
    r.settle();
    for (i, p) in r.peers.iter().enumerate() {
        assert!(p.doc().slides == done.slides, "peer {i}: {n} undos then {n} redos leave the deck unchanged");
    }
    r.assert_converged();
}

#[test]
fn undo_of_my_delete_leaves_a_slide_a_peer_deleted_since() {
    let mut r = Room::new(basic(), 2, 10);
    let a = find(&r.peers[0].doc(), "alpha");
    let slide = r.peers[0].slide_ids()[0];
    r.peers[0].run("edit.delete", json!({"ids": [a.0]})).expect("delete");
    r.settle();
    r.peers[1].run("slide.go", json!({"index": 0})).expect("go");
    r.peers[1].run("slide.delete", json!({"index": 0})).expect("delete slide");
    r.settle();
    r.peers[0].notices.clear();
    undo(&mut r, 0);
    r.settle();
    for (i, p) in r.peers.iter().enumerate() {
        assert!(!p.slide_ids().contains(&slide), "peer {i}: the slide the other editor deleted stays deleted");
        assert!(shape_of(&p.doc(), a).is_none(), "peer {i}: nothing of it comes back");
    }
    assert!(r.peers[0].notices.iter().any(|n| n.text == UNDO_SKIPPED_TEXT), "I am told what was not undone");
    r.assert_converged();
}

#[test]
fn undo_whose_typing_a_peer_deleted_leaves_my_older_box() {
    let mut r = Room::new(basic(), 2, 11);
    let a = find(&r.peers[0].doc(), "alpha");
    let b = find(&r.peers[0].doc(), "beta");
    // My older step: typing in beta.
    r.peers[0].run("text.edit", json!({"id": b.0})).expect("edit");
    r.peers[0].run("text.insert", json!({"text": "Older"})).expect("type");
    // My latest step: typing in alpha, which the other editor then takes out again.
    r.peers[0].run("text.edit", json!({"id": a.0})).expect("edit");
    r.peers[0].run("text.insert", json!({"text": "Z"})).expect("type");
    r.settle();
    r.peers[1].run("text.set", json!({"id": a.0, "text": "alpha"})).expect("set");
    r.settle();
    let older = text_of(&r, 1, b);
    assert!(older.contains("Older"), "my older typing is there: {older:?}");
    r.peers[0].notices.clear();
    undo(&mut r, 0);
    r.settle();
    for i in 0..2 {
        assert!(text_of(&r, i, b) == older, "peer {i}: my older box keeps its text");
        assert!(text_of(&r, i, a) == "alpha", "peer {i}: the other editor's text stays");
    }
    assert!(r.peers[0].notices.iter().any(|n| n.text == UNDO_SKIPPED_TEXT), "I am told what was not undone");
    r.assert_converged();
}

#[test]
fn undo_of_a_restore_deletes_it_again() {
    let mut r = Room::new(basic(), 2, 12);
    let a = find(&r.peers[0].doc(), "alpha");
    r.peers[1].run("edit.delete", json!({"ids": [a.0]})).expect("delete");
    r.settle();
    let p = &mut r.peers[0];
    p.live.restore(&mut p.session, Tomb::Shape(a)).expect("restore");
    r.settle();
    assert!(shape_of(&r.peers[1].doc(), a).is_some(), "the Restore reached the other peer");
    undo(&mut r, 0);
    r.settle();
    for (i, p) in r.peers.iter().enumerate() {
        assert!(shape_of(&p.doc(), a).is_none(), "peer {i}: undoing my Restore deletes it again");
    }
    r.assert_converged();
}

#[test]
fn the_undo_hook_answers_whether_there_was_a_step() {
    let mut r = Room::new(basic(), 2, 13);
    let a = find(&r.peers[0].doc(), "alpha");
    let none = r.peers[0].session.execute("edit.undo", &json!({})).expect("undo");
    assert!(none["taken"] == json!(false), "nothing to undo yet: {none}");
    r.settle();
    r.peers[0].run("shape.fill", json!({"ids": [a.0], "color": "#C00000"})).expect("fill");
    let some = r.peers[0].session.execute("edit.undo", &json!({})).expect("undo");
    assert!(some["taken"] == json!(true), "a step to undo: {some}");
    r.settle();
    r.assert_converged();
}

#[test]
fn commands_made_right_after_undo_in_one_batch_are_dropped_and_said() {
    let mut r = Room::new(basic(), 2, 14);
    let a = find(&r.peers[0].doc(), "alpha");
    let (fill0, x0) = (fill_of(&r, 0, a), x_of(&r, 0, a));
    // A host running commands on the session between pumps: one batch of fill, Undo, move.
    let s = &mut r.peers[0].session;
    s.execute("shape.fill", &json!({"ids": [a.0], "color": "#C00000"})).expect("fill");
    s.execute("edit.undo", &json!({})).expect("undo");
    s.execute("shape.move", &json!({"ids": [a.0], "dx": 40.0})).expect("move");
    r.settle();
    assert!(r.peers[0].notices.iter().any(|n| n.text == AFTER_UNDO_TEXT), "I am told the edit was not kept");
    for i in 0..2 {
        assert!(fill_of(&r, i, a) == fill0, "peer {i}: the fill is undone");
        assert!(x_of(&r, i, a) == x0, "peer {i}: the move made after the Undo is not kept");
    }
    r.assert_converged();
}
