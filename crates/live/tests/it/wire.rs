//! Values that hold a live-minted id (at or above `0x8000_0000`) read alike on every peer, though
//! yrs sends such an integer as a float: a connector's ends (a shape value), a shape's click link
//! (a shape value) and a run's link (a text attribute), each naming something minted live.

use craft_collab::Replica;
use deckcraft_live::deck_from_state;
use deckcraft_model::text::{Action, RunKind};
use deckcraft_model::{LIVE_ID_BASE, Presentation, ShapeId, ShapeKind, SlideId};
use serde_json::{Value, json};

use crate::support::{Room, basic, find};

fn id_of(v: &Value) -> u32 {
    v.get("id").and_then(Value::as_u64).expect("an id") as u32
}

/// The slide a run link in `shape` points at.
fn run_link(p: &Presentation, shape: ShapeId) -> Option<SlideId> {
    let sh = p.slides.iter().find_map(|s| s.shape(shape))?;
    sh.text.as_ref()?.paragraphs.iter().flat_map(|para| para.runs.iter()).find_map(|r| match (&r.kind, &r.props.link) {
        (RunKind::Text, Some(l)) => match l.action {
            Action::Slide { slide } => Some(slide),
            _ => None,
        },
        _ => None,
    })
}

#[test]
fn values_naming_live_minted_ids_read_alike_on_every_peer() {
    let mut r = Room::new(basic(), 2, 7);
    let a = find(&r.peers[0].doc(), "alpha");
    // A slide minted live, then back to the first slide.
    r.peers[0].run("slide.new", json!({"layout": "titleOnly", "title": "Live"})).expect("slide");
    let live_slide = r.peers[0].doc().slides.iter().map(|s| s.id).find(|id| id.0 >= LIVE_ID_BASE).expect("a live-minted slide");
    let at = r.peers[0].doc().slides.iter().position(|s| s.id == live_slide).expect("index");
    r.peers[0].run("slide.go", json!({"index": 0})).expect("go");
    // Two shapes minted live, joined by a connector.
    let left = id_of(&r.peers[0].run("shape.insert", json!({"preset": "rect", "rect": [100.0, 380.0, 120.0, 60.0]})).expect("left"));
    let right = id_of(&r.peers[0].run("shape.insert", json!({"preset": "ellipse", "rect": [500.0, 380.0, 120.0, 60.0]})).expect("right"));
    let joint = ShapeId(id_of(&r.peers[0].run("shape.connect", json!({"from": left, "to": right})).expect("connect")));
    assert!(left >= LIVE_ID_BASE && right >= LIVE_ID_BASE && joint.0 >= LIVE_ID_BASE, "minted live");
    // The shape `left` links to the live slide on click (a shape value)...
    r.peers[0].run("insert.hyperlink", json!({"slide": at, "ids": [left]})).expect("shape link");
    // ...and alpha's text links to it (a run attribute).
    r.peers[0].run("text.edit", json!({"id": a.0})).expect("edit");
    r.peers[0].run("text.selectAll", json!({})).expect("select");
    r.peers[0].run("insert.hyperlink", json!({"slide": at})).expect("run link");
    r.peers[0].run("edit.deselect", json!({})).expect("deselect");
    r.settle();
    r.assert_converged();

    let author = r.peers[0].doc();
    assert_eq!(run_link(&author, a), Some(live_slide), "the author's run links to the live slide");
    let reload = deck_from_state(r.peers[0].live.base(), &r.peers[0].live.replica().expect("replica").state()).expect("reload");
    for (who, p) in [("peer 1", (*r.peers[1].doc()).clone()), ("a reload of the state", reload)] {
        assert!(p == *author, "{who} equals the author");
        let ShapeKind::Connector { start: Some((from, _)), end: Some((to, _)) } = p.slides[0].shape(joint).expect("the connector").kind else {
            panic!("{who}: the connector keeps both ends")
        };
        assert_eq!((from.0, to.0), (left, right), "{who}: its ends name the live shapes");
        let click = p.slides[0].shape(ShapeId(left)).and_then(|s| s.click.clone()).map(|l| l.action);
        assert!(matches!(click, Some(Action::Slide { slide }) if slide == live_slide), "{who}: the shape's link names the live slide");
        assert_eq!(run_link(&p, a), Some(live_slide), "{who}: the run's link names the live slide");
    }
}
