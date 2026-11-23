//! The command gate, the safety net and the start checks (L4).

use std::collections::BTreeSet;
use std::sync::Arc;

use craft_live::Access;
use craft_live::testing::{MemRelay, ROOM};
use deckcraft_engine::Session;
use deckcraft_live::gate::{MEDIA, NOT_YET, tables, tables_holding};
use deckcraft_live::{BaseFile, CLOSED_TEXT, COPY_UNSHARED_TEXT, DeckLive, LiveFail, NOT_YET_TEXT, SECTIONS_TEXT};
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

#[test]
fn minting_before_a_space_is_granted_is_refused() {
    let bytes = basic();
    let relay = MemRelay::new();
    let hub = relay.connect("user0", "User 0", true);
    let mut session = Session::new();
    let base = BaseFile { bytes: &bytes, name: NAME, expected_sha256: None };
    let mut live = DeckLive::start(hub, ROOM, &base, Access::Edit, "user0", &mut session).expect("start");
    let doc = |s: &Session| s.active().expect("doc").doc.clone();
    let before = doc(&session);
    let seq = relay.seq();
    // Not joined yet: the gate says so, and the engine itself mints nothing.
    assert!(live.run(&mut session, "shape.insert", &json!({"preset": "rect"})) == Err(craft_live::CONNECTING.to_owned()), "the gate waits");
    let direct = session.execute("shape.insert", &json!({"preset": "rect"}));
    assert!(direct.is_err(), "no id is minted before the grant");
    assert!(Arc::ptr_eq(&doc(&session), &before), "the deck is unchanged");
    live.pump(&mut session);
    assert!(relay.seq() == seq, "nothing was sent");
    // Once joined, the same command mints from the granted space.
    for _ in 0..3 {
        live.pump(&mut session);
    }
    assert!(live.run(&mut session, "shape.insert", &json!({"preset": "rect"})).is_ok(), "after the grant it runs");
    assert!(relay.seq() > seq, "and is shared");
}

#[test]
fn deleting_an_animated_shape_passes_the_net() {
    let bytes = deck_bytes(|s| {
        add_box(s, "moving", [60.0, 60.0, 200.0, 80.0]);
        add_box(s, "trigger", [320.0, 60.0, 200.0, 80.0]);
        let doc = s.active().expect("doc").doc.clone();
        let (m, t) = (find(&doc, "moving"), find(&doc, "trigger"));
        s.execute("animation.add", &json!({"effect": "fade", "ids": [m.0]})).expect("animate");
        s.execute("animation.add", &json!({"effect": "spin", "ids": [t.0]})).expect("animate");
        s.execute("animation.timing", &json!({"index": 0, "trigger": t.0})).expect("triggered");
    });
    let mut r = Room::new(bytes, 2, 25);
    let doc = r.peers[0].doc();
    let (m, t) = (find(&doc, "moving"), find(&doc, "trigger"));
    let n = doc.slides[0].animations.len();
    r.peers[0].run("edit.delete", json!({"ids": [t.0]})).expect("the delete is shared");
    r.settle();
    for (i, p) in r.peers.iter().enumerate() {
        let s = &p.doc().slides[0];
        assert!(s.shape(t).is_none(), "peer {i}: the shape is gone");
        assert!(s.animations.len() < n, "peer {i}: its animation went with it");
        assert!(s.animations.iter().all(|a| a.shape != t && a.trigger != Some(t)), "peer {i}: nothing names it");
        assert!(s.animations.iter().any(|a| a.shape == m), "peer {i}: the other shape keeps its animation");
    }
    r.assert_converged();
}

#[test]
fn slide_moves_in_a_deck_with_sections_say_why() {
    let bytes = deck_bytes(|s| {
        s.execute("slide.new", &json!({})).expect("slide");
        s.execute("section.add", &json!({"name": "Part", "at": 1})).expect("section");
    });
    let mut r = Room::new(bytes, 2, 26);
    assert!(!r.peers[0].doc().sections.is_empty(), "the base has sections");
    let before = r.peers[0].doc();
    let got = r.peers[0].run("slide.move", json!({"from": 0, "to": 1}));
    assert!(got == Err(SECTIONS_TEXT.to_owned()), "refused with the reason: {got:?}");
    assert!(*r.peers[0].doc() == *before, "the deck is put back");
}

#[test]
fn closing_the_live_presentation_ends_the_session() {
    let mut r = Room::new(basic(), 2, 27);
    r.peers[0].run("file.close", json!({})).expect("close");
    assert!(r.peers[0].session.documents().is_empty(), "the presentation is closed");
    assert!(!r.peers[0].live.editable(), "the link is over");
    assert!(r.peers[0].run("shape.insert", json!({})) == Err(CLOSED_TEXT.to_owned()), "nothing runs after it");
    let seq = r.relay.seq();
    for _ in 0..3 {
        r.pump_all();
    }
    assert!(r.relay.seq() == seq, "nothing more is sent");
    // The others carry on.
    let a = find(&r.peers[1].doc(), "alpha");
    r.peers[1].run("shape.move", json!({"ids": [a.0], "dx": 10.0})).expect("the other peer still edits");
}
