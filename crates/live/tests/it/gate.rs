//! The command gate, the safety net and the start checks (L4).

use std::collections::BTreeSet;
use std::sync::Arc;

use craft_live::Access;
use craft_live::testing::{MemRelay, ROOM};
use deckcraft_engine::Session;
use deckcraft_live::gate::{MEDIA, NOT_YET, tables, tables_holding};
use deckcraft_live::{BaseFile, COPY_UNSHARED_TEXT, DeckLive, LiveFail, NOT_YET_TEXT};
use serde_json::json;

use crate::support::{NAME, Room, add_box, basic, deck_bytes, find, shape_of};

#[test]
fn every_registered_command_is_classified_once() {
    let registered: BTreeSet<&str> = Session::new().commands().iter().map(|c| c.id).collect();
    for id in &registered {
        assert!(tables_holding(id) == 1, "{id} is in exactly one gate table");
    }
    for t in tables() {
        for id in t {
            assert!(registered.contains(id), "{id} in a gate table is a registered command");
        }
    }
}

#[test]
fn every_blocked_command_is_refused_and_the_deck_is_unchanged() {
    let mut r = Room::new(basic(), 2, 21);
    let before = r.peers[0].doc();
    let seq = r.relay.seq();
    for id in NOT_YET.iter().chain(MEDIA) {
        assert!(r.peers[0].run(id, json!({})).is_err(), "{id} is refused");
        assert!(Arc::ptr_eq(&r.peers[0].doc(), &before), "{id} left the deck alone");
    }
    assert!(r.relay.seq() == seq, "nothing was sent");
}

#[test]
fn safety_net_puts_back_an_unshared_change() {
    let mut r = Room::new(basic(), 2, 22);
    let before = r.peers[0].doc();
    let seq = r.relay.seq();
    // Past the gate, as a panel calling the engine directly would.
    r.peers[0].session.execute("design.slideSize", &json!({"preset": "standard"})).expect("resize");
    assert!(*r.peers[0].doc() != *before, "the engine changed the deck");
    r.pump(0);
    assert!(*r.peers[0].doc() == *before, "the change is put back");
    assert!(r.peers[0].notices.iter().any(|n| n.text == NOT_YET_TEXT), "the person is told why");
    assert!(r.relay.seq() == seq, "nothing was sent");
    r.settle();
    r.assert_converged();
}

#[test]
fn duplicate_of_an_animated_slide_is_refused_with_the_reason() {
    let bytes = deck_bytes(|s| {
        add_box(s, "moving", [60.0, 60.0, 200.0, 80.0]);
        let id = find(&s.active().expect("doc").doc, "moving");
        s.execute("edit.select", &json!({"ids": [id.0]})).expect("select");
        s.execute("animation.add", &json!({"effect": "fade"})).expect("animate");
    });
    let mut r = Room::new(bytes, 2, 23);
    assert!(!r.peers[0].doc().slides[0].animations.is_empty(), "the base slide is animated");
    let before = r.peers[0].doc();
    let seq = r.relay.seq();
    let got = r.peers[0].run("slide.duplicate", json!({"index": 0}));
    assert!(got == Err(COPY_UNSHARED_TEXT.to_owned()), "refused with the reason: {got:?}");
    assert!(*r.peers[0].doc() == *before, "no copy is left behind");
    assert!(r.relay.seq() == seq, "nothing was sent");
}

#[test]
fn read_only_access_sends_nothing() {
    let mut r = Room::with_access(basic(), &[Access::Edit, Access::Read], 24);
    let a = find(&r.peers[1].doc(), "alpha");
    let theirs = r.peers[0].doc();
    let seq = r.relay.seq();
    assert!(r.peers[1].run("shape.fill", json!({"ids": [a.0], "color": "#C00000"})).is_err(), "a reader's edit is refused");
    // Past the gate: the mapping puts it back.
    let fill = shape_of(&r.peers[1].doc(), a).and_then(|s| s.fill);
    r.peers[1].session.execute("shape.fill", &json!({"ids": [a.0], "color": "#C00000"})).expect("fill");
    r.settle();
    assert!(r.relay.seq() == seq, "nothing was sent");
    assert!(shape_of(&r.peers[1].doc(), a).and_then(|s| s.fill) == fill, "the reader's deck is put back");
    assert!(*r.peers[0].doc() == *theirs, "the editor's deck is unchanged");
    // The reader still sees the editor's changes.
    r.peers[0].run("shape.move", json!({"ids": [a.0], "dx": 40.0})).expect("move");
    r.settle();
    assert!(*r.peers[1].doc() == *r.peers[0].doc(), "the reader follows the editor");
}

#[test]
fn base_mismatch_refuses_the_start() {
    let bytes = basic();
    let relay = MemRelay::new();
    let hub = relay.connect("user0", "User 0", true);
    let mut session = Session::new();
    let base = BaseFile { bytes: &bytes, name: NAME, expected_sha256: Some([7; 32]) };
    let got = DeckLive::start(hub, ROOM, &base, Access::Edit, "user0", &mut session);
    assert!(matches!(got, Err(LiveFail::BaseMismatch)), "a different file does not start");
    assert!(session.documents().is_empty(), "nothing was opened");
}

#[test]
fn too_large_opens_alone() {
    // Text that compresses well in the file but is over the seed limit once expanded.
    let long = "z".repeat(3 * 1024 * 1024);
    let bytes = deck_bytes(|s| {
        for i in 0..3 {
            add_box(s, &long, [40.0 + 200.0 * f64::from(i), 100.0, 180.0, 120.0]);
        }
    });
    let relay = MemRelay::new();
    let hub = relay.connect("user0", "User 0", true);
    let mut session = Session::new();
    let base = BaseFile { bytes: &bytes, name: NAME, expected_sha256: None };
    let got = DeckLive::start(hub, ROOM, &base, Access::Edit, "user0", &mut session);
    assert!(matches!(got, Err(LiveFail::TooLarge)), "over the limit, no live session");
    assert!(session.documents().len() == 1, "the file is open for this person alone");
    assert!(relay.seq() == 0, "nothing was sent");
}
