//! `DeckDoc`: the deckcraft/1 schema, its seed, the rebuild and the split plan (L3).

use std::sync::Arc;

use craft_collab::Replica;
use craft_collab::yrs::Update;
use craft_collab::yrs::updates::decoder::Decode;
use deckcraft_engine::Session;

use deckcraft_live::order::between;
use deckcraft_live::text::{Chunk, TextOp, edit_ops};
use deckcraft_live::{
    DeckChange, DeckDoc, DeckSeed, KeyRef, MAX_SEED_BYTES, PlannedWrite, TextTarget, Tomb, deck_from_doc, deck_from_state, plan_split,
};
use deckcraft_model::style::PictureFill;
use deckcraft_model::table::Table;
use deckcraft_model::text::TextBody;
use deckcraft_model::{LIVE_ID_BASE, Presentation, Shape, ShapeId, ShapeKind, Slide, Xfrm, defaults};
use serde_json::json;

/// The relay frame: no update may pass it once encoded.
const FRAME: usize = 1024 * 1024;

fn shape(p: &mut Presentation, name: &str, x: f64, y: f64, w: f64, h: f64) -> Shape {
    Shape { id: ShapeId(p.alloc_id().expect("id")), name: name.into(), xfrm: Some(Xfrm::new(x, y, w, h)), ..Default::default() }
}

/// Nested groups, a glued connector, a picture, a table, a chart, text and notes, built in code.
fn mixed() -> Presentation {
    let mut p = defaults::new_presentation(None);
    let media = p.add_media("pic.png", "image/png", vec![0x89, b'P', b'N', b'G']).expect("media");
    let layout = defaults::layout_of_kind(&p, deckcraft_model::LayoutType::Blank).unwrap_or_default();
    let mut s = defaults::new_slide(&mut p, layout).expect("slide");
    let mut pic = shape(&mut p, "Picture", 20.0, 10.0, 80.0, 40.0);
    pic.kind = ShapeKind::Picture { fill: PictureFill { media, ..Default::default() } };
    let mut inner = shape(&mut p, "Inner", 10.0, 60.0, 60.0, 30.0);
    inner.text = Some(TextBody::from_text("Inside\nthe group"));
    let mut nested = shape(&mut p, "Nested", 10.0, 10.0, 200.0, 100.0);
    nested.kind = ShapeKind::Group { children: vec![inner], child: Xfrm::new(0.0, 0.0, 200.0, 100.0) };
    let mut group = shape(&mut p, "Group", 100.0, 150.0, 400.0, 180.0);
    group.kind = ShapeKind::Group { children: vec![pic, nested], child: Xfrm::new(0.0, 0.0, 400.0, 180.0) };
    group.tags = vec![("k".into(), "v".into())];
    let left = shape(&mut p, "Left", 40.0, 400.0, 80.0, 40.0);
    let right = shape(&mut p, "Right", 240.0, 400.0, 80.0, 40.0);
    let mut cx = shape(&mut p, "Connector", 120.0, 420.0, 120.0, 0.0);
    cx.kind = ShapeKind::Connector { start: Some((left.id, 3)), end: Some((right.id, 1)) };
    cx.geom = deckcraft_model::Geom::preset("straightConnector1");
    let mut table = Table::new(3, 3, 360.0, 28.0);
    if let Some(cell) = table.cell_mut(0, 0) {
        cell.text = TextBody::from_text("Cell");
    }
    let mut ts = shape(&mut p, "Table", 40.0, 320.0, 360.0, 84.0);
    ts.kind = ShapeKind::Table(table);
    let mut chart = shape(&mut p, "Chart", 500.0, 20.0, 300.0, 200.0);
    chart.kind = ShapeKind::Chart(Box::default());
    let mut title = shape(&mut p, "Title", 10.0, 10.0, 300.0, 40.0);
    title.text = Some(TextBody::from_text("Héllo 😀 deck"));
    s.shapes.extend([title, group, left, right, cx, ts, chart]);
    s.notes = TextBody::from_text("Speaker notes\nsecond line");
    s.has_notes_slide = true;
    // First, before the slide the new deck starts with; a blank slide last.
    p.slides.insert(0, Arc::new(s));
    let blank = defaults::new_slide(&mut p, layout).expect("slide");
    p.slides.push(Arc::new(blank));
    p
}

/// The sample deck as the engine builds it, the same through a .pptx round trip, every starter
/// and the mixed deck.
fn decks() -> Vec<(String, Presentation)> {
    let mut s = Session::new();
    deckcraft_engine::sample::open_sample(&mut s).expect("sample");
    let sample = s.doc().expect("doc").doc.as_ref().clone();
    let imported = deckcraft_pptx::import(&deckcraft_pptx::export(&sample).expect("export")).expect("import");
    let mut out = vec![("sample".to_owned(), sample), ("sample.pptx".to_owned(), imported), ("mixed".to_owned(), mixed())];
    for starter in deckcraft_engine::templates::starters() {
        out.push((starter.name.to_owned(), deckcraft_engine::templates::build(starter.name, defaults::WIDE).expect("starter")));
    }
    out
}

fn seed_of(p: &Presentation) -> DeckSeed {
    DeckSeed { base_sha256: [7; 32], deck: Arc::new(p.clone()) }
}

/// A peer joined from the seed's state, minting in `space`.
fn peer(seeded: &DeckDoc, space: u32) -> DeckDoc {
    let mut d = DeckDoc::from_state(&seeded.state()).expect("state");
    d.set_client(space).expect("space");
    d
}

fn all_shapes(p: &Presentation) -> Vec<ShapeId> {
    let mut out = Vec::new();
    for s in &p.slides {
        deckcraft_model::walk(&s.shapes, &mut |sh, _| out.push(sh.id));
    }
    out
}

#[test]
fn seed_is_deterministic() {
    for (name, base) in decks() {
        let a = DeckDoc::seeded(&seed_of(&base)).expect("seed");
        let b = DeckDoc::seeded(&seed_of(&base)).expect("seed");
        // yrs writes map values in hash order, so bytes may differ (L2): compare the items.
        let (ua, ub) = (Update::decode_v1(&a.state()).expect("a"), Update::decode_v1(&b.state()).expect("b"));
        assert_eq!(ua.state_vector(), ub.state_vector(), "{name}: clocks");
        assert_eq!(ua.insertions(true), ub.insertions(true), "{name}: item ids");
        assert_eq!(a.state().len(), b.state().len(), "{name}: size");
        // An edit made against b's items lands the same on a: every entry, every text.
        let mut pb = peer(&b, 9);
        let ids = all_shapes(&base);
        let update = pb
            .write(false, |t| {
                for id in &ids {
                    t.set_shape(*id, "name", json!(format!("n{}", id.0)));
                }
                Ok(())
            })
            .expect("write");
        let texts = pb
            .write(true, |t| {
                for id in &ids {
                    t.text_ops(TextTarget::Shape(*id), &[TextOp::Insert { at: 1, chunks: vec![Chunk::Text { text: "x".into(), attrs: vec![] }] }]);
                }
                for s in &base.slides {
                    t.text_ops(TextTarget::Notes(s.id), &[TextOp::Insert { at: 0, chunks: vec![Chunk::Text { text: "y".into(), attrs: vec![] }] }]);
                }
                Ok(())
            })
            .expect("text");
        let mut ra = peer(&a, 3);
        let mut rb = peer(&b, 4);
        for r in [&mut ra, &mut rb] {
            r.apply(&update).expect("apply");
            r.apply(&texts).expect("apply");
        }
        assert_eq!(deck_from_doc(&base, &ra), deck_from_doc(&base, &rb), "{name}: an edit lands alike");
        assert_eq!(deck_from_doc(&base, &ra), deck_from_doc(&base, &pb), "{name}: and as its author has it");
    }
}

#[test]
fn seed_rebuilds_to_the_base() {
    for (name, base) in decks() {
        let doc = DeckDoc::seeded(&seed_of(&base)).expect("seed");
        assert_eq!(deck_from_state(&base, &doc.state()).expect("rebuild"), base, "{name}");
        assert_eq!(deck_from_doc(&base, &doc), base, "{name}");
        assert_eq!(doc.base_sha256(), Some([7; 32]));
        assert_eq!(doc.slide_order(), base.slides.iter().map(|s| s.id).collect::<Vec<_>>(), "{name}");
    }
}

#[test]
fn rebuild_keeps_next_id_below_the_live_range() {
    let base = mixed();
    let seeded = DeckDoc::seeded(&seed_of(&base)).expect("seed");
    let mut a = peer(&seeded, 5);
    let slide = base.slides[0].id;
    let live = ShapeId(LIVE_ID_BASE | 5 << 16);
    let mut extra = Presentation { id_block: deckcraft_model::IdBlock::new(5), ..base.clone() };
    let id = ShapeId(extra.alloc_id().expect("block id"));
    assert_eq!(id, live);
    let new = Shape { id, name: "Live".into(), xfrm: Some(Xfrm::new(1.0, 2.0, 3.0, 4.0)), ..Default::default() };
    a.write(false, |t| {
        t.put_shape(slide, None, &between(None, None, 5), &new);
        Ok(())
    })
    .expect("put");
    let mut rebuilt = deck_from_state(&base, &a.state()).expect("rebuild");
    assert!(rebuilt.slides[0].shape(id).is_some(), "the live shape is in the deck");
    assert_eq!(rebuilt.next_id, base.next_id);
    let block_less = rebuilt.alloc_id().expect("a block-less id");
    assert!(block_less < LIVE_ID_BASE, "{block_less:#x}");
    rebuilt.fix_next_id();
    assert!(rebuilt.next_id < LIVE_ID_BASE, "reload keeps the counter below the live range");
}

#[test]
fn a_deck_over_the_seed_limit_says_so() {
    let small = mixed();
    assert!(DeckDoc::seeded(&seed_of(&small)).expect("seed").seed_bytes() < MAX_SEED_BYTES);
    let mut big = mixed();
    let long = "x".repeat(40 * 1024);
    let mut s = Slide::clone(&big.slides[0]);
    for i in 0..260 {
        let mut sh = shape(&mut big, &format!("Box {i}"), 0.0, 0.0, 10.0, 10.0);
        sh.text = Some(TextBody::from_text(&long));
        s.shapes.push(sh);
    }
    big.slides[0] = Arc::new(s);
    let doc = DeckDoc::seeded(&seed_of(&big)).expect("seed");
    assert!(doc.seed_bytes() > MAX_SEED_BYTES, "{}", doc.seed_bytes());
}

#[test]
fn remote_changes_name_what_moved() {
    let base = mixed();
    let seeded = DeckDoc::seeded(&seed_of(&base)).expect("seed");
    let (mut a, mut b) = (peer(&seeded, 3), peer(&seeded, 4));
    let title = base.slides[0].shapes[0].id;
    let slide = base.slides[1].id;
    let u = a.write(false, |t| {
        t.set_shape(title, "fill", json!(null));
        Ok(())
    });
    assert_eq!(b.apply(&u.expect("write")).expect("apply"), vec![DeckChange::Shapes(vec![title])]);
    let ops = vec![TextOp::Insert { at: 0, chunks: vec![Chunk::Text { text: "Hi ".into(), attrs: vec![] }] }];
    let u = a.write(true, |t| {
        t.text_ops(TextTarget::Shape(title), &ops);
        Ok(())
    });
    assert_eq!(b.apply(&u.expect("write")).expect("apply"), vec![DeckChange::Text(title)]);
    let u = a.write(true, |t| {
        t.text_ops(TextTarget::Notes(slide), &ops);
        Ok(())
    });
    assert_eq!(b.apply(&u.expect("write")).expect("apply"), vec![DeckChange::Notes(slide)]);
    let first = b.slide(base.slides[0].id).expect("slide").pos;
    let u = a.write(false, |t| {
        t.set_slide(slide, "pos", json!(between(None, Some(&first), 3)));
        Ok(())
    });
    assert_eq!(b.apply(&u.expect("write")).expect("apply"), vec![DeckChange::Slides(vec![slide])]);
    assert_eq!(b.slide_order().first(), Some(&slide), "the moved slide is first");
    let u = a.write(false, |t| {
        t.set_del(Tomb::Shape(title), true, "alice");
        Ok(())
    });
    assert_eq!(b.apply(&u.expect("write")).expect("apply"), vec![DeckChange::Shapes(vec![title])]);
    let e = b.shape(title).expect("entry");
    assert!(e.del);
    assert_eq!(e.del_by.as_deref(), Some("alice"));
    assert!(!b.shapes_of(base.slides[0].id).contains(&title));
    assert!(deck_from_doc(&base, &b).slides[1].shape(title).is_none(), "a tombstone is not in the deck");
    assert_eq!(deck_from_doc(&base, &a), deck_from_doc(&base, &b));
}

#[test]
fn split_updates_stay_under_the_budget() {
    let base = mixed();
    let seeded = DeckDoc::seeded(&seed_of(&base)).expect("seed");
    let (mut author, mut receiver) = (peer(&seeded, 6), peer(&seeded, 8));
    let slide = base.slides.last().expect("blank slide").id;
    let mut deck = Presentation { id_block: deckcraft_model::IdBlock::new(6), ..base.clone() };
    let long: String = "Pasted text with some length. ".repeat(40);
    let mut z: Option<String> = None;
    let mut writes = Vec::new();
    for i in 0..2000 {
        let mut s = shape(&mut deck, &format!("Paste {i}"), f64::from(i % 50) * 10.0, 5.0, 40.0, 20.0);
        s.text = Some(TextBody::from_text(&format!("{i} {long}")));
        let key = between(z.as_deref(), None, 6);
        writes.push(PlannedWrite::PutShape { slide, parent: None, z: key.clone(), shape: Box::new(s) });
        z = Some(key);
    }
    let parts = plan_split(&writes);
    assert!(parts.len() > 1, "a paste this large is cut");
    assert_eq!(parts.iter().map(Vec::len).sum::<usize>(), writes.len(), "nothing lost or doubled");
    for part in &parts {
        let u = author
            .write(false, |t| {
                for w in part {
                    w.write(t);
                }
                Ok(())
            })
            .expect("write");
        assert!(u.len() < FRAME, "{} bytes", u.len());
        receiver.apply(&u).expect("apply");
    }
    let rebuilt = deck_from_doc(&base, &receiver);
    assert_eq!(rebuilt, deck_from_doc(&base, &author));
    assert_eq!(receiver.shapes_of(slide).len(), 2000);
}

#[test]
fn a_shape_s_writes_stay_in_one_part() {
    let base = mixed();
    let title = base.slides[0].shapes[0].id;
    let mut writes = Vec::new();
    for i in 0..400u32 {
        let id = ShapeId(1000 + i);
        writes.push(PlannedWrite::SetShape { id, key: "descr".into(), value: json!("d".repeat(4000)) });
    }
    let before = base.slides[0].shapes[0].text.clone().unwrap_or_default();
    let mut after = before.clone();
    after.paragraphs[0].runs[0].text.push_str(&"t".repeat(5000));
    writes.push(PlannedWrite::SetShape { id: title, key: "name".into(), value: json!("T") });
    writes.push(PlannedWrite::Text { target: TextTarget::Shape(title), ops: edit_ops(&before.paragraphs, &after.paragraphs) });
    for i in 0..400u32 {
        writes.push(PlannedWrite::SetShape { id: ShapeId(1000 + i), key: "title".into(), value: json!("t".repeat(4000)) });
    }
    let parts = plan_split(&writes);
    assert!(parts.len() > 1);
    let holding = |id: ShapeId| {
        parts
            .iter()
            .filter(|p| {
                p.iter().any(|w| {
                    matches!(w, PlannedWrite::SetShape { id: i, .. } if *i == id)
                        || matches!(w, PlannedWrite::Text { target: TextTarget::Shape(i), .. } if *i == id)
                })
            })
            .count()
    };
    assert_eq!(holding(title), 1, "the title's key and text travel together");
    for i in 0..400u32 {
        assert_eq!(holding(ShapeId(1000 + i)), 1);
    }
}

#[test]
fn junk_remote_values_do_not_brick_the_room() {
    let base = mixed();
    let seeded = DeckDoc::seeded(&seed_of(&base)).expect("seed");
    let (mut a, mut b, mut c) = (peer(&seeded, 3), peer(&seeded, 4), peer(&seeded, 5));
    let slide = &base.slides[0];
    let id = slide.shapes[0].id;
    let group = slide.shapes.iter().find(|s| s.is_group()).expect("group");
    let child = group.children()[0].id;
    let u = a
        .write(false, |t| {
            t.set_shape(id, "xfrm", json!(Xfrm::new(11.0, 12.0, 13.0, 14.0)));
            Ok(())
        })
        .expect("write");
    b.apply(&u).expect("apply");
    c.apply(&u).expect("apply");
    let junk = c
        .write(false, |t| {
            t.set_shape(id, "xfrm", json!("x"));
            t.set_shape(id, "z", json!(7));
            t.set_shape(id, "kind", json!({"type": "garbage", "what": [1, 2]}));
            t.set_shape(id, "del", json!("maybe"));
            t.set_shape(id, "slide", json!("nowhere"));
            t.set_shape(id, "body", json!(5));
            t.set_shape(id, "text", json!(42));
            t.set_shape(child, "parent", json!([1]));
            t.set_slide(slide.id, "pos", json!(9));
            t.set_slide(slide.id, "layout", json!("l"));
            t.set_slide(slide.id, "del", json!({}));
            Ok(())
        })
        .expect("junk");
    assert!(b.apply(&junk).is_ok(), "junk is not a refused update");
    let live = deck_from_doc(&base, &b);
    // History-free: a replica built from the state alone reads the same (Minor 2).
    assert_eq!(live, deck_from_state(&base, &b.state()).expect("rebuild"), "live and rebuilt agree after junk");
    // Every junk key, core keys too, reads as the base's (Minor 3).
    assert_eq!(live.slides.iter().map(|s| s.id).collect::<Vec<_>>(), base.slides.iter().map(|s| s.id).collect::<Vec<_>>());
    let (got, want) = (&live.slides[0], &base.slides[0]);
    assert_eq!(got.layout, want.layout);
    assert_eq!(got.shapes.iter().map(|s| s.id).collect::<Vec<_>>(), want.shapes.iter().map(|s| s.id).collect::<Vec<_>>(), "same shapes, same order");
    let (gs, ws) = (got.shape(id).expect("shape"), want.shape(id).expect("shape"));
    assert_eq!((gs.xfrm, &gs.kind, &gs.text), (ws.xfrm, &ws.kind, &ws.text), "junk reads as the base's");
    assert!(got.shape(group.id).expect("group").children().iter().any(|s| s.id == child), "the child stays in its group");
    // The next good write applies.
    let better = Xfrm::new(21.0, 22.0, 23.0, 24.0);
    let u = c
        .write(false, |t| {
            t.set_shape(id, "xfrm", json!(better));
            Ok(())
        })
        .expect("write");
    assert_eq!(b.apply(&u).expect("apply"), vec![DeckChange::Shapes(vec![id])]);
    assert_eq!(deck_from_doc(&base, &b).slides[0].shape(id).and_then(|s| s.xfrm), Some(better));
}

/// A shape with no text body: both peers start typing into it at once. The YText exists from the
/// seed, so both texts merge (L3 fix round 1, Important 1).
#[test]
fn two_peers_starting_a_text_body_keep_both_texts() {
    let base = mixed();
    let left = base.slides[0].shapes.iter().find(|s| s.name == "Left").expect("left");
    assert!(left.text.is_none(), "the shape starts without a text body");
    let seeded = DeckDoc::seeded(&seed_of(&base)).expect("seed");
    let (mut a, mut b) = (peer(&seeded, 3), peer(&seeded, 4));
    let body = |who: &str| TextBody::from_text(who);
    let start = |d: &mut DeckDoc, who: &str| -> Vec<Vec<u8>> {
        let after = body(who);
        let key = d
            .write(false, |t| {
                t.set_shape(left.id, "body", json!({"body": after.body}));
                Ok(())
            })
            .expect("body");
        let text = d
            .write(true, |t| {
                t.text_ops(TextTarget::Shape(left.id), &edit_ops(&[], &after.paragraphs));
                Ok(())
            })
            .expect("text");
        vec![key, text]
    };
    let from_a = start(&mut a, "alice");
    let from_b = start(&mut b, "bob");
    for u in &from_b {
        a.apply(u).expect("apply");
    }
    for u in &from_a {
        b.apply(u).expect("apply");
    }
    let (da, db) = (deck_from_doc(&base, &a), deck_from_doc(&base, &b));
    assert_eq!(da, db, "both peers converge");
    let text = da.slides[0].shape(left.id).and_then(|s| s.text.as_ref()).map(TextBody::text).expect("a text body");
    assert!(text.contains("alice") && text.contains("bob"), "both texts survive: {text:?}");
}

/// Undoing the first keystroke into a new text body keeps a peer's later text (Important 1).
#[test]
fn undoing_the_first_keystroke_keeps_a_peer_s_later_text() {
    let base = mixed();
    let right = base.slides[0].shapes.iter().find(|s| s.name == "Right").expect("right");
    let seeded = DeckDoc::seeded(&seed_of(&base)).expect("seed");
    let (mut a, mut b) = (peer(&seeded, 3), peer(&seeded, 4));
    let target = TextTarget::Shape(right.id);
    let first = a
        .write(true, |t| {
            t.text_ops(target.clone(), &[TextOp::Insert { at: 0, chunks: vec![Chunk::Text { text: "a\n".into(), attrs: vec![] }] }]);
            Ok(())
        })
        .expect("type");
    b.apply(&first).expect("apply");
    let later = b
        .write(true, |t| {
            t.text_ops(target.clone(), &[TextOp::Insert { at: 1, chunks: vec![Chunk::Text { text: " from bob".into(), attrs: vec![] }] }]);
            Ok(())
        })
        .expect("type");
    a.apply(&later).expect("apply");
    let undo = a.undo_text().expect("undo");
    b.apply(&undo).expect("apply");
    for d in [&a, &b] {
        let text: String = d.text(target.clone()).expect("text").iter().map(|p| p.text()).collect();
        assert!(text.contains("from bob"), "bob's text stays: {text:?}");
        assert!(!text.starts_with('a'), "alice's keystroke is undone: {text:?}");
    }
}

/// `f` writes and then errs: the update for what it wrote is still handed out, the receiver
/// converges, and later updates from that client do not wait (Important 2).
#[test]
fn a_write_that_errs_after_writing_still_hands_out_its_update() {
    let base = mixed();
    let title = base.slides[0].shapes[0].id;
    let seeded = DeckDoc::seeded(&seed_of(&base)).expect("seed");
    let (mut a, mut b) = (peer(&seeded, 3), peer(&seeded, 4));
    let failed = a.write(false, |t| {
        t.set_shape(title, "name", json!("half"));
        Err(craft_collab::LiveError::OutOfRange)
    });
    let err = failed.expect_err("f erred");
    assert_eq!(err.error, craft_collab::LiveError::OutOfRange);
    let update = err.update.expect("what f wrote is handed out");
    b.apply(&update).expect("apply");
    let refused = a.write(false, |_| Err(craft_collab::LiveError::OutOfRange)).expect_err("erred");
    assert!(refused.update.is_none(), "nothing written, nothing to send");
    let next = a
        .write(false, |t| {
            t.set_shape(title, "descr", json!("after"));
            Ok(())
        })
        .expect("write");
    assert_eq!(b.apply(&next).expect("apply"), vec![DeckChange::Shapes(vec![title])], "the next update lands at once");
    assert!(!b.needs_resync());
    assert_eq!(deck_from_doc(&base, &a), deck_from_doc(&base, &b));
}

#[test]
fn garbage_update_is_refused_and_the_doc_stays_sound() {
    let base = mixed();
    let seeded = DeckDoc::seeded(&seed_of(&base)).expect("seed");
    let (mut a, mut b) = (peer(&seeded, 3), peer(&seeded, 4));
    let state = b.state();
    let deck = deck_from_doc(&base, &b);
    let mut junk: Vec<Vec<u8>> = vec![vec![], vec![0xff; 64], vec![1, 2, 3, 4, 5, 6, 7, 8, 9], b"not an update".to_vec()];
    let real = a
        .write(false, |t| {
            t.set_shape(base.slides[0].shapes[0].id, "name", json!("renamed"));
            Ok(())
        })
        .expect("write");
    junk.push(real.iter().map(|b| b ^ 0x5a).collect());
    junk.push(real[..real.len() / 2].to_vec());
    for bytes in &junk {
        let _ = b.apply(bytes);
        assert!(!b.damaged(), "{bytes:?}");
    }
    assert!(b.apply(&[0xff; 64]).is_err(), "garbage is refused");
    assert_eq!(deck_from_doc(&base, &b), deck, "nothing changed");
    assert_eq!(Update::decode_v1(&b.state()).expect("state").state_vector(), Update::decode_v1(&state).expect("state").state_vector());
    assert_eq!(b.apply(&real).expect("apply"), vec![DeckChange::Shapes(vec![base.slides[0].shapes[0].id])]);
    assert_eq!(deck_from_doc(&base, &a), deck_from_doc(&base, &b));
    let u = b.write(false, |t| {
        t.set_slide(base.slides[0].id, "hidden", json!(true));
        Ok(())
    });
    assert!(u.is_ok(), "the replica still takes edits");
}

#[test]
fn sticky_carets_follow_remote_typing() {
    let base = mixed();
    let title = base.slides[0].shapes[0].id;
    let seeded = DeckDoc::seeded(&seed_of(&base)).expect("seed");
    let (mut a, mut b) = (peer(&seeded, 3), peer(&seeded, 4));
    let target = TextTarget::Shape(title);
    // "Héllo 😀 deck": the caret after the emoji, char 7.
    let s = b.sticky(target.clone(), 7).expect("sticky");
    assert_eq!(b.resolve(target.clone(), &s), Some(7));
    let u = a.write(true, |t| {
        t.text_ops(target.clone(), &[TextOp::Insert { at: 0, chunks: vec![Chunk::Text { text: "ünï ".into(), attrs: vec![] }] }]);
        Ok(())
    });
    b.apply(&u.expect("write")).expect("apply");
    assert_eq!(b.resolve(target.clone(), &s), Some(11), "the caret moved with the text before it");
    assert_eq!(b.resolve(TextTarget::Notes(base.slides[0].id), &s), None, "a sticky index names its own text");
    assert_eq!(b.resolve(target, b"junk"), None);
}

#[test]
fn text_undo_takes_back_only_tracked_writes() {
    let base = mixed();
    let title = base.slides[0].shapes[0].id;
    let seeded = DeckDoc::seeded(&seed_of(&base)).expect("seed");
    let mut a = peer(&seeded, 3);
    let start = a.text(TextTarget::Shape(title)).expect("text");
    a.write(false, |t| {
        t.set_shape(title, "name", json!("Kept"));
        Ok(())
    })
    .expect("write");
    a.write(true, |t| {
        t.text_ops(TextTarget::Shape(title), &[TextOp::Insert { at: 0, chunks: vec![Chunk::Text { text: "Typed ".into(), attrs: vec![] }] }]);
        Ok(())
    })
    .expect("write");
    assert_ne!(a.text(TextTarget::Shape(title)), Some(start.clone()));
    assert!(a.undo_text().is_some());
    assert_eq!(a.text(TextTarget::Shape(title)), Some(start));
    assert!(a.undo_text().is_none(), "the name write is not in the text history");
    assert_eq!(a.shape(title).expect("entry").fields.get("name"), Some(&json!("Kept")));
    assert!(a.redo_text().is_some());
    assert!(a.undo().is_none(), "craft-live's own counter never undoes behind the log");
    let read = a.write(false, |t| {
        assert_eq!(t.read(KeyRef::Shape(title, "name".into())), Some(json!("Kept")));
        assert_eq!(t.read(KeyRef::Shape(title, "text".into())), None);
        Ok(())
    });
    assert!(read.is_ok());
}

#[test]
fn group_children_follow_their_parent() {
    let base = mixed();
    let seeded = DeckDoc::seeded(&seed_of(&base)).expect("seed");
    let mut a = peer(&seeded, 3);
    let slide = &base.slides[0];
    let group = slide.shapes.iter().find(|s| s.is_group()).expect("group");
    let order = a.shapes_of(slide.id);
    let gi = order.iter().position(|i| *i == group.id).expect("group");
    for child in group.children() {
        let ci = order.iter().position(|i| *i == child.id).expect("child");
        assert!(ci > gi, "a child follows its group");
        assert_eq!(a.shape(child.id).expect("entry").parent, Some(group.id));
    }
    // A loop of parents reads at slide level rather than hanging or vanishing.
    let child = group.children()[1].id;
    a.write(false, |t| {
        t.set_shape(group.id, "parent", json!(child.0));
        Ok(())
    })
    .expect("write");
    let deck = deck_from_doc(&base, &a);
    let mut seen = 0;
    deckcraft_model::walk(&deck.slides[0].shapes, &mut |s, _| seen += usize::from(s.id == group.id || s.id == child));
    assert_eq!(seen, 2, "both are still in the deck once");
    // Deleting the group takes its children with it, as edit.delete does.
    a.write(false, |t| {
        t.set_shape(group.id, "parent", json!(0));
        t.set_del(Tomb::Shape(group.id), true, "bob");
        Ok(())
    })
    .expect("write");
    let deck = deck_from_doc(&base, &a);
    for id in std::iter::once(group.id).chain(group.children().iter().map(|c| c.id)) {
        assert!(deck.slides[0].shape(id).is_none());
    }
}
