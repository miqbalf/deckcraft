//! Remote changes held for a gesture: the stale-twist budget, and the link ending when the
//! replica fell behind for good (L4 fix round 1).

use craft_collab::{CRDT_TEXT, ClientMsg, Hub, Replica, TextPayload};
use craft_live::LiveStatus;
use craft_live::testing::ROOM;
use deckcraft_live::DeckDoc;
use serde_json::json;

use crate::support::{Room, basic, find, shape_of};

/// Peer 0 starts a twist of alpha; peer 1 recolours beta, which peer 0 holds.
fn twist_with_a_held_change(seed: u64) -> (Room, deckcraft_model::ShapeId, Option<deckcraft_model::style::Fill>) {
    let mut r = Room::new(basic(), 2, seed);
    let doc = r.peers[0].doc();
    let (a, b) = (find(&doc, "alpha"), find(&doc, "beta"));
    let fill_b = shape_of(&doc, b).and_then(|s| s.fill);
    r.peers[0].run("edit.select", json!({"ids": [a.0]})).expect("select");
    r.peers[0].run("shape.rotate", json!({"by": 5.0, "gesture": "move"})).expect("twist");
    r.peers[1].run("shape.fill", json!({"ids": [b.0], "color": "#00A000"})).expect("fill");
    for _ in 0..3 {
        r.pump_all();
    }
    assert!(r.peers[0].live.holding(), "the remote change waits for the twist");
    (r, b, fill_b)
}

fn twisting(r: &Room) -> bool {
    let uid = r.peers[0].live.doc();
    r.peers[0].session.remote_busy(uid)
}

#[test]
fn a_stale_twist_is_ended_and_the_held_change_lands() {
    let (mut r, b, fill_b) = twist_with_a_held_change(31);
    // The person goes to another presentation; the twist's end never comes.
    // (Added without a command: a command would settle the twist itself.)
    let doc = deckcraft_engine::Session::with_new().active().expect("a new presentation").clone();
    let other = Some(r.peers[0].session.add_document(doc));
    assert!(twisting(&r), "the twist is still open");
    r.tick(11_000);
    for _ in 0..3 {
        r.pump_all();
    }
    assert!(!twisting(&r), "the stale twist was ended");
    assert!(r.peers[0].session.active_index() == other, "the active presentation stays the person's");
    assert!(!r.peers[0].live.holding(), "nothing is held any more");
    assert!(shape_of(&r.peers[0].doc(), b).and_then(|s| s.fill) != fill_b, "the held change landed");
    r.settle();
    r.assert_converged();
}

#[test]
fn a_long_twist_still_turning_is_not_cut() {
    let (mut r, b, fill_b) = twist_with_a_held_change(32);
    // Turning on for well past the budget, a step every two seconds.
    for _ in 0..12 {
        r.tick(2_000);
        r.peers[0].session.execute("shape.rotate", &json!({"by": 5.0, "gesture": "move"})).expect("twist");
        r.pump_all();
        assert!(twisting(&r), "the twist still turning is not cut");
        assert!(r.peers[0].live.holding(), "the remote change still waits");
    }
    r.peers[0].run("shape.rotate", json!({"gesture": "end"})).expect("end");
    r.settle();
    assert!(shape_of(&r.peers[0].doc(), b).and_then(|s| s.fill) != fill_b, "the held change landed after the twist");
    r.assert_converged();
}

#[test]
fn a_replica_that_fell_behind_for_good_ends_the_link() {
    let mut r = Room::new(basic(), 2, 33);
    let a = find(&r.peers[0].doc(), "alpha");
    // Another client's updates whose first one never reaches the room: they wait for it until
    // past the replica's bound, and it needs the room's full state.
    let mut rogue = DeckDoc::from_state(&r.peers[0].live.replica().expect("replica").state()).expect("state");
    rogue.set_client(30_000).expect("space");
    let mut write = |s: String| {
        rogue.write(false, |t| {
            t.set_shape(a, "name", json!(s));
            Ok(())
        })
    };
    let _lost = write("first".into()).expect("first");
    let later: Vec<Vec<u8>> = (0..4).map(|i| write(format!("{i}").repeat(400 * 1024)).expect("later")).collect();
    let mut hub = r.relay.connect("rogue", "Rogue", true);
    for (i, u) in later.iter().enumerate() {
        let params = serde_json::to_value(TextPayload::new(u)).expect("payload");
        hub.send(ClientMsg::Submit { doc: ROOM.into(), local: i as u64 + 1, command: CRDT_TEXT.into(), params, scope: None, fence: None })
            .expect("send");
    }
    for _ in 0..4 {
        r.pump_all();
    }
    for (i, p) in r.peers.iter().enumerate() {
        assert!(p.live.replica().is_some_and(DeckDoc::needs_resync), "peer {i}'s replica is behind");
        assert!(matches!(p.live.status(), LiveStatus::Broken(_)), "peer {i}'s link ended");
        assert!(!p.live.editable(), "peer {i} edits nothing more");
    }
}
