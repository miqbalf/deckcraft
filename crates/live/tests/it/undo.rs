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

/// The Resync after commands dropped behind an Undo (L6 review M8): the window is rebuilt from the
/// replica at once, also where the Undo itself changed nothing (a dropped move of another shape),
/// the notice is said once, and the next edit is shared as usual.
#[test]
fn the_window_is_rebuilt_after_commands_dropped_behind_an_undo() {
    let mut r = Room::new(basic(), 2, 15);
    let (a, b) = (find(&r.peers[0].doc(), "alpha"), find(&r.peers[0].doc(), "beta"));
    let x0 = x_of(&r, 0, b);
    let s = &mut r.peers[0].session;
    s.execute("shape.fill", &json!({"ids": [a.0], "color": "#C00000"})).expect("fill");
    s.execute("edit.undo", &json!({})).expect("undo");
    s.execute("shape.move", &json!({"ids": [b.0], "dx": 40.0})).expect("move");
    r.pump(0);
    assert!(x_of(&r, 0, b) == x0, "the dropped move is gone from the window after the same pump");
    assert!(*r.peers[0].doc() == r.peers[0].rebuilt(), "the window shows the replica");
    r.settle();
    let said = r.peers[0].notices.iter().filter(|n| n.text == AFTER_UNDO_TEXT).count();
    assert_eq!(said, 1, "told once");
    r.peers[0].run("shape.move", json!({"ids": [b.0], "dx": 20.0})).expect("move again");
    r.settle();
    assert!(x_of(&r, 1, b) > x0, "the next edit is shared");
    r.assert_converged();
}

/// The lock a peer holds lapses and goes to another (L6 review M8, U2 fix-1 concern 3, room
/// level): its edit under the old grant is refused and taken back out of its replica, which
/// craft-live restarts the mapping for (`reset`). My undo step for that edit goes with it, and
/// both windows agree. DeckCraft M1 takes no locks itself; the test takes one on the core.
#[test]
fn a_taken_back_edit_takes_my_undo_steps_with_it() {
    const SCOPE: &str = "deck/struct";
    let mut r = Room::new(basic(), 2, 17);
    let a = find(&r.peers[0].doc(), "alpha");
    let fill0 = fill_of(&r, 1, a);
    r.peers[0].live.core_mut().lock(SCOPE).expect("lock");
    r.pump(0);
    let granted = r.peers[0].live.core_mut().take_lock_events();
    assert!(matches!(granted.last(), Some(craft_collab::LockEvent::Granted { token: Some(_), .. })), "{granted:?}");
    // A pauses past its lease: the scope goes to B.
    r.relay.lapse(SCOPE);
    r.peers[1].live.core_mut().lock(SCOPE).expect("lock");
    r.pump(1);
    r.peers[1].live.core_mut().take_lock_events();
    // A edits under its old grant: refused by the relay, taken back.
    r.peers[0].run("shape.fill", json!({"ids": [a.0], "color": "#C00000"})).expect("fill");
    assert!(r.peers[0].live.can_undo(), "a step to undo before the take-back");
    r.settle();
    assert!(!r.peers[0].live.can_undo(), "the step went with the replaced replica");
    assert!(fill_of(&r, 0, a) == fill0 && fill_of(&r, 1, a) == fill0, "the refused fill is gone everywhere");
    r.assert_converged();
}

/// DC1 (LP1's guard, DeckCraft's form): an Undo never takes away another editor's text. Ann adds
/// a box, Bob types in it, Ann undoes the box: it becomes a tombstone that still holds Bob's
/// text (Ruling D8), Bob is told who removed it and offered Restore, and Restore brings his text
/// back on both pages. Text undo takes back only Ann's own characters (`undo_reverts_only_my_text`).
#[test]
fn undo_never_takes_away_another_editors_text() {
    let mut r = Room::new(basic(), 2, 3);
    let before: Vec<ShapeId> = r.peers[0].doc().slides[0].shapes.iter().map(|s| s.id).collect();
    r.peers[0].run("shape.insert", json!({"preset": "rect", "rect": [400.0, 300.0, 200.0, 80.0], "text": "Ann"})).expect("insert");
    r.peers[0].run("text.exit", json!({})).ok();
    r.settle();
    let b = r.peers[1].doc().slides[0].shapes.iter().map(|s| s.id).find(|id| !before.contains(id)).expect("Bob sees Ann's box");
    r.peers[1].run("text.edit", json!({"id": b.0})).expect("edit");
    r.peers[1].run("text.insert", json!({"text": "Bob "})).expect("type");
    r.peers[1].run("text.exit", json!({})).ok();
    r.settle();
    undo(&mut r, 0);
    r.settle();
    for i in 0..2 {
        assert!(shape_of(&r.peers[i].doc(), b).is_none(), "peer {i}: Ann's undo removed her box from view");
    }
    let replica = r.peers[1].live.replica().expect("replica");
    let kept = replica.text(deckcraft_live::TextTarget::Shape(b)).map(|ps| ps.iter().map(|p| p.text()).collect::<String>()).unwrap_or_default();
    assert!(kept.contains("Bob"), "the tombstone still holds Bob's text: {kept:?}");
    assert!(r.peers[1].notices.iter().any(|n| n.restore == Some(Tomb::Shape(b))), "Bob is offered Restore: {:?}", r.peers[1].notices);
    let p = &mut r.peers[1];
    p.live.restore(&mut p.session, Tomb::Shape(b)).expect("restore");
    r.settle();
    for i in 0..2 {
        let t = shape_of(&r.peers[i].doc(), b).and_then(|s| s.text.map(|t| t.text())).unwrap_or_default();
        assert!(t.contains("Bob"), "peer {i}: Bob's text is back: {t:?}");
    }
    r.assert_converged();
}
