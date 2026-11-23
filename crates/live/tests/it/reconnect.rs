//! Editing while reconnecting (C9): every M1 edit is free, so a peer whose link dropped keeps
//! editing, and its edits reach the room when the link returns. A reset that would drop them
//! first hands over an unmerged copy.

use chrono::NaiveDateTime;
use craft_live::LiveStatus;
use deckcraft_live::{Tomb, UNMERGED_TEXT, unmerged_name};
use deckcraft_model::ShapeId;
use serde_json::json;

use crate::support::{NAME, Room, basic, find, shape_of};

fn text_of(r: &Room, peer: usize, id: ShapeId) -> String {
    shape_of(&r.peers[peer].doc(), id).and_then(|s| s.text).map(|t| t.text()).unwrap_or_default()
}

fn x_of(r: &Room, peer: usize, id: ShapeId) -> f64 {
    shape_of(&r.peers[peer].doc(), id).and_then(|s| s.xfrm).map(|x| x.x).expect("placed shape")
}

#[test]
fn edits_made_while_reconnecting_reach_the_room() {
    for seed in 1..4 {
        let mut r = Room::new(basic(), 2, seed);
        let doc = r.peers[0].doc();
        let (a, b, g) = (find(&doc, "alpha"), find(&doc, "beta"), find(&doc, "gamma"));
        let x0 = x_of(&r, 1, b);
        r.drop_link(0);
        for _ in 0..3 {
            r.pump_all();
        }
        assert!(matches!(r.peers[0].live.status(), LiveStatus::Reconnecting), "{:?}", r.peers[0].live.status());
        assert!(r.peers[0].live.editable(), "free edits go on while reconnecting");
        r.peers[0].run("text.edit", json!({"id": a.0})).expect("edit offline");
        r.peers[0].run("text.insert", json!({"text": "offline "})).expect("type offline");
        r.peers[0].run("shape.move", json!({"ids": [b.0], "dx": 40.0})).expect("move offline");
        r.peers[1].run("shape.fill", json!({"ids": [g.0], "color": "#C00000"})).expect("the other peer edits meanwhile");
        for _ in 0..3 {
            r.pump_all();
        }
        assert!(!text_of(&r, 1, a).contains("offline"), "nothing reached the room while the link was down");
        r.restore_link(0);
        r.settle();
        assert!(text_of(&r, 1, a).contains("offline"), "the offline text arrived (seed {seed})");
        assert!(x_of(&r, 1, b) > x0, "the offline move arrived (seed {seed})");
        assert!(
            shape_of(&r.peers[0].doc(), g).and_then(|s| s.fill) == shape_of(&r.peers[1].doc(), g).and_then(|s| s.fill),
            "and the room's edit reached A"
        );
        r.assert_converged();
    }
}

#[test]
fn a_reset_after_offline_edits_offers_the_unmerged_copy() {
    let mut r = Room::new(basic(), 2, 4);
    let a = find(&r.peers[0].doc(), "alpha");
    assert!(r.peers[0].live.take_unmerged().is_none(), "nothing to hand over while in step");
    r.drop_link(0);
    r.pump_all();
    r.peers[0].run("text.edit", json!({"id": a.0})).expect("edit offline");
    r.peers[0].run("text.insert", json!({"text": "kept offline "})).expect("type offline");
    r.pump_all();
    // The link returns, and the relay answers with the room's snapshot instead of the replay:
    // the replica restarts from it, which would drop the offline edits.
    r.restore_link(0);
    r.relay.fall_behind(r.peers[0].live.core().hub());
    r.pump(0);
    let (name, bytes) = r.peers[0].live.take_unmerged().expect("the unmerged copy");
    let (head, tail) = ("Deck (unmerged ", ").pptx");
    assert!(name.starts_with(head) && name.ends_with(tail), "{name}");
    let at = NaiveDateTime::parse_from_str(&name[head.len()..name.len() - tail.len()], "%Y-%m-%d %H%M").expect("a date and time");
    assert_eq!(name, unmerged_name(NAME, at), "named by unmerged_name");
    let copy = deckcraft_engine::cmd::file::open_presentation(&name, &bytes).expect("the copy opens");
    let t = shape_of(&copy, a).and_then(|s| s.text).map(|t| t.text()).unwrap_or_default();
    assert!(t.contains("kept offline") && t.contains("alpha"), "the copy holds the offline text: {t:?}");
    assert!(r.peers[0].live.take_unmerged().is_none(), "taken once");
    // The drop itself. A snapshot after local edits cannot be merged into, so craft-live gives
    // the replica up (C9): A's link ends and A edits no more. (Here the rejoin had resent the
    // edits before the snapshot came, so the room holds them too; the copy is the safety net for
    // when it did not.) Reopened, A converges with the room.
    r.settle();
    assert!(matches!(r.peers[0].live.status(), LiveStatus::Broken(_)), "{:?}", r.peers[0].live.status());
    assert!(!r.peers[0].live.editable(), "the given-up replica takes no edits");
    let again = r.join(&basic());
    assert!(*r.peers[again].doc() == *r.peers[1].doc(), "the reopened window equals the room");
    assert!(*r.peers[again].doc() == r.peers[1].rebuilt(), "and the room's replica");
}

#[test]
fn unmerged_names_keep_the_stem() {
    let at = NaiveDateTime::parse_from_str("2026-11-22 1404", "%Y-%m-%d %H%M").expect("date");
    assert_eq!(unmerged_name("Deck.pptx", at), "Deck (unmerged 2026-11-22 1404).pptx");
    assert_eq!(unmerged_name("dir/Q3 plan.v2.pptx", at), "Q3 plan.v2 (unmerged 2026-11-22 1404).pptx");
}

/// Offline text, then the snapshot reset: the edits, as an unmerged copy.
fn offline_then_reset(r: &mut Room, a: ShapeId, text: &str) {
    r.drop_link(0);
    r.pump_all();
    r.peers[0].run("text.edit", json!({"id": a.0})).expect("edit offline");
    r.peers[0].run("text.insert", json!({"text": text})).expect("type offline");
    r.pump_all();
    r.restore_link(0);
    r.relay.fall_behind(r.peers[0].live.core().hub());
    r.pump(0);
    r.settle();
}

fn copy_text(bytes: &[u8], a: ShapeId) -> String {
    let copy = deckcraft_engine::cmd::file::open_presentation(NAME, bytes).expect("the copy opens");
    shape_of(&copy, a).and_then(|s| s.text).map(|t| t.text()).unwrap_or_default()
}

/// A copy that waits is told once and kept, whatever hands it over again (craft-live's own
/// hand-over, then this session's on close): one copy with the offline text.
#[test]
fn a_waiting_copy_is_told_and_kept_across_hand_overs() {
    let mut r = Room::new(basic(), 2, 13);
    let a = find(&r.peers[0].doc(), "alpha");
    offline_then_reset(&mut r, a, "first ");
    let told = |r: &Room| r.peers[0].notices.iter().filter(|n| n.text == UNMERGED_TEXT).count();
    assert_eq!(told(&r), 1, "told a copy is waiting");
    r.peers[0].run("file.close", json!({})).ok();
    r.pump_all();
    assert_eq!(told(&r), 1, "told once while it waits");
    let (_, bytes) = r.peers[0].live.take_unmerged().expect("the copy");
    assert!(copy_text(&bytes, a).contains("first"), "the copy keeps the offline text");
    assert!(r.peers[0].live.take_unmerged().is_none(), "taken once");
}

/// Closing the live presentation while edits wait for the link: they are kept as a copy.
#[test]
fn closing_while_reconnecting_keeps_the_offline_edits() {
    let mut r = Room::new(basic(), 2, 14);
    let a = find(&r.peers[0].doc(), "alpha");
    r.drop_link(0);
    r.pump_all();
    r.peers[0].run("text.edit", json!({"id": a.0})).expect("edit offline");
    r.peers[0].run("text.insert", json!({"text": "unsent "})).expect("type offline");
    r.peers[0].run("file.close", json!({})).expect("close");
    let (_, bytes) = r.peers[0].live.take_unmerged().expect("the copy");
    assert!(copy_text(&bytes, a).contains("unsent"), "the copy holds the unsent text");
}

/// An offline edit to a shape a peer deletes meanwhile: after the rejoin it is offered back, and
/// Restore brings it back with the offline text everywhere.
#[test]
fn an_offline_edit_to_a_shape_a_peer_deleted_offers_restore() {
    let mut r = Room::new(basic(), 2, 15);
    let a = find(&r.peers[0].doc(), "alpha");
    r.drop_link(0);
    r.pump_all();
    r.peers[0].run("text.edit", json!({"id": a.0})).expect("edit offline");
    r.peers[0].run("text.insert", json!({"text": "away "})).expect("type offline");
    r.peers[0].run("edit.deselect", json!({})).ok();
    r.peers[1].run("edit.delete", json!({"ids": [a.0]})).expect("delete");
    r.pump_all();
    r.restore_link(0);
    r.settle();
    r.assert_converged();
    assert!(r.peers[0].notices.iter().any(|n| n.restore == Some(Tomb::Shape(a))), "offered Restore: {:?}", r.peers[0].notices);
    let p = &mut r.peers[0];
    p.live.restore(&mut p.session, Tomb::Shape(a)).expect("restore");
    r.settle();
    for i in 0..2 {
        assert!(text_of(&r, i, a).contains("away"), "peer {i}: back with the offline text");
    }
    r.assert_converged();
}

/// Offline edits past the buffer bound: the gate and Restore say the buffer is full.
#[test]
fn a_full_offline_buffer_says_so() {
    let mut r = Room::new(basic(), 2, 16);
    let doc = r.peers[0].doc();
    let (a, g) = (find(&doc, "alpha"), find(&doc, "gamma"));
    r.peers[1].run("edit.delete", json!({"ids": [g.0]})).expect("delete");
    r.settle();
    r.drop_link(0);
    r.pump_all();
    r.peers[0].run("text.edit", json!({"id": a.0})).expect("edit offline");
    let big = "x".repeat(400_000);
    for _ in 0..8 {
        if !r.peers[0].live.editable() {
            break;
        }
        r.peers[0].run("text.insert", json!({"text": big})).expect("type offline");
    }
    assert_eq!(r.peers[0].live.allows("shape.move"), Err(craft_live::BUFFER_FULL), "the gate names the full buffer");
    let p = &mut r.peers[0];
    let refused = p.live.restore(&mut p.session, Tomb::Shape(g)).expect_err("restore waits too");
    assert_eq!(refused, craft_live::BUFFER_FULL);
    assert!(p.live.replica().is_some_and(|d| d.shape(g).is_some_and(|e| e.del)), "nothing was written");
}

/// Stopping the session while edits wait for the link: stop hands back the copy.
#[test]
fn stopping_while_reconnecting_returns_the_offline_edits() {
    let mut r = Room::new(basic(), 2, 17);
    let a = find(&r.peers[0].doc(), "alpha");
    r.drop_link(0);
    r.pump_all();
    r.peers[0].run("text.edit", json!({"id": a.0})).expect("edit offline");
    r.peers[0].run("text.insert", json!({"text": "unsent "})).expect("type offline");
    let p = r.peers.remove(0);
    let (live, mut session) = (p.live, p.session);
    let (_, bytes) = live.stop(&mut session).expect("stop returns the copy");
    assert!(copy_text(&bytes, a).contains("unsent"), "the copy holds the unsent text");
}

/// The copy was taken after the break; closing afterwards adds no second copy and no second line.
#[test]
fn closing_after_the_copy_was_taken_makes_no_second_copy() {
    let mut r = Room::new(basic(), 2, 18);
    let a = find(&r.peers[0].doc(), "alpha");
    offline_then_reset(&mut r, a, "first ");
    assert!(r.peers[0].live.take_unmerged().is_some(), "the copy");
    r.peers[0].run("file.close", json!({})).ok();
    r.pump_all();
    assert!(r.peers[0].live.take_unmerged().is_none(), "nothing new arrived since");
    assert_eq!(r.peers[0].notices.iter().filter(|n| n.text == UNMERGED_TEXT).count(), 1, "told once");
}

/// Closing while live right after an edit: the edit is on its way, not "could not be merged".
#[test]
fn closing_while_live_right_after_an_edit_keeps_no_copy() {
    let mut r = Room::new(basic(), 2, 19);
    let a = find(&r.peers[0].doc(), "alpha");
    r.peers[0].run("text.edit", json!({"id": a.0})).expect("edit");
    r.peers[0].run("text.insert", json!({"text": "quick "})).expect("type");
    r.peers[0].run("file.close", json!({})).expect("close");
    assert!(r.peers[0].live.take_unmerged().is_none(), "no copy");
    assert!(r.peers[0].notices.iter().all(|n| n.text != UNMERGED_TEXT), "no 'could not be merged'");
    for _ in 0..4 {
        r.pump(1);
    }
    assert!(text_of(&r, 1, a).contains("quick"), "the edit reached the room");
}

/// The link drops after the last pump and the person types and closes before the next one: the
/// status still says Live, but the edit never left, so it comes back as the copy (re-review 2 I2).
#[test]
fn closing_right_after_the_link_dropped_keeps_the_edit() {
    let mut r = Room::new(basic(), 2, 20);
    let a = find(&r.peers[0].doc(), "alpha");
    r.drop_link(0);
    r.peers[0].run("text.edit", json!({"id": a.0})).expect("edit");
    r.peers[0].run("text.insert", json!({"text": "dropped "})).expect("type");
    r.peers[0].run("file.close", json!({})).expect("close");
    let (_, bytes) = r.peers[0].live.take_unmerged().expect("the copy");
    assert!(copy_text(&bytes, a).contains("dropped"), "the copy holds the text");
    assert!(r.peers[0].notices.iter().any(|n| n.text == UNMERGED_TEXT), "told");
}

/// The same window, ended by stop(): stop returns the copy.
#[test]
fn stopping_right_after_the_link_dropped_returns_the_edit() {
    let mut r = Room::new(basic(), 2, 21);
    let a = find(&r.peers[0].doc(), "alpha");
    r.drop_link(0);
    r.peers[0].run("text.edit", json!({"id": a.0})).expect("edit");
    r.peers[0].run("text.insert", json!({"text": "dropped "})).expect("type");
    let p = r.peers.remove(0);
    let (live, mut session) = (p.live, p.session);
    let (_, bytes) = live.stop(&mut session).expect("stop returns the copy");
    assert!(copy_text(&bytes, a).contains("dropped"), "the copy holds the text");
}

/// The link drops and comes back between two pumps, so the status never left Live and the hub
/// says connected again (a stale status, L5 re-review 3). An edit made then and closed at once is
/// never lost: it reaches the room, or it comes back as the copy.
#[test]
fn a_drop_and_rejoin_between_pumps_loses_no_edit() {
    let mut r = Room::new(basic(), 2, 22);
    let a = find(&r.peers[0].doc(), "alpha");
    r.drop_link(0);
    r.restore_link(0);
    assert!(matches!(r.peers[0].live.status(), LiveStatus::Live { .. }), "the status still says Live");
    r.peers[0].run("text.edit", json!({"id": a.0})).expect("edit");
    r.peers[0].run("text.insert", json!({"text": "rejoined "})).expect("type");
    r.peers[0].run("file.close", json!({})).expect("close");
    let copy = r.peers[0].live.take_unmerged();
    for _ in 0..8 {
        r.tick(16);
        r.pump(1);
    }
    let reached = text_of(&r, 1, a).contains("rejoined");
    let kept = copy.as_ref().is_some_and(|(_, bytes)| copy_text(bytes, a).contains("rejoined"));
    assert!(reached || kept, "the edit reached the room ({reached}) or is in the copy ({kept})");
}

/// The page reopened the file in another room while this window was offline and typing: the
/// host ends the session, and the offline edits come back as the copy (U2 review C1).
#[test]
fn a_host_end_while_offline_keeps_the_offline_edits() {
    let mut r = Room::new(basic(), 2, 23);
    let a = find(&r.peers[0].doc(), "alpha");
    r.drop_link(0);
    r.pump_all();
    r.peers[0].run("text.edit", json!({"id": a.0})).expect("edit");
    r.peers[0].run("text.insert", json!({"text": "offline "})).expect("type");
    let p = &mut r.peers[0];
    p.live.end(&mut p.session, "The live session was restarted. Reopen the file.");
    r.pump(0);
    let (_, bytes) = r.peers[0].live.take_unmerged().expect("the copy");
    assert!(copy_text(&bytes, a).contains("offline"), "the copy holds the offline text");
    assert!(matches!(r.peers[0].live.status(), LiveStatus::Broken(_)), "the link ended");
}

/// The same with the link up: an edit sent to a room that is being left may never be ordered,
/// so it is kept too.
#[test]
fn a_host_end_right_after_an_edit_keeps_it() {
    let mut r = Room::new(basic(), 2, 24);
    let a = find(&r.peers[0].doc(), "alpha");
    r.peers[0].run("text.edit", json!({"id": a.0})).expect("edit");
    r.peers[0].run("text.insert", json!({"text": "unconfirmed "})).expect("type");
    let p = &mut r.peers[0];
    p.live.end(&mut p.session, "Your access changed. Reopen the file.");
    r.pump(0);
    let (_, bytes) = r.peers[0].live.take_unmerged().expect("the copy");
    assert!(copy_text(&bytes, a).contains("unconfirmed"), "the copy holds the edit");
}

/// A host end after everything was confirmed makes no copy.
#[test]
fn a_host_end_with_nothing_unconfirmed_makes_no_copy() {
    let mut r = Room::new(basic(), 2, 25);
    let a = find(&r.peers[0].doc(), "alpha");
    r.peers[0].run("text.edit", json!({"id": a.0})).expect("edit");
    r.peers[0].run("text.insert", json!({"text": "shared "})).expect("type");
    r.settle();
    let p = &mut r.peers[0];
    p.live.end(&mut p.session, "Your access changed. Reopen the file.");
    r.pump(0);
    assert!(r.peers[0].live.take_unmerged().is_none(), "no copy");
}
