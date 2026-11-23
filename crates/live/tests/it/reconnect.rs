//! Editing while reconnecting (C9): every M1 edit is free, so a peer whose link dropped keeps
//! editing, and its edits reach the room when the link returns. A reset that would drop them
//! first hands over an unmerged copy.

use chrono::NaiveDateTime;
use craft_live::LiveStatus;
use deckcraft_live::unmerged_name;
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
}

#[test]
fn unmerged_names_keep_the_stem() {
    let at = NaiveDateTime::parse_from_str("2026-11-22 1404", "%Y-%m-%d %H%M").expect("date");
    assert_eq!(unmerged_name("Deck.pptx", at), "Deck (unmerged 2026-11-22 1404).pptx");
    assert_eq!(unmerged_name("dir/Q3 plan.v2.pptx", at), "Q3 plan.v2 (unmerged 2026-11-22 1404).pptx");
}
