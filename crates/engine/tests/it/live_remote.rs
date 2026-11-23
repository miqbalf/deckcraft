//! Remote changes (H2) and the live undo hook (H3): what someone else changed lands with no undo
//! step, no journal entry and no report; the selection follows ids; a drag or a composition holds
//! it off; Undo and Redo of a live presentation go to the live layer.

use std::sync::{Arc, Mutex};

use deckcraft_engine::model::{Presentation, ShapeId, ShapeKind, Slide, SlideId};
use deckcraft_engine::{EngineError, Mods, PointerEvent, PointerKind, Session};
use serde_json::json;

type Seen = Arc<Mutex<usize>>;

/// Counts the reports of document `uid`.
fn observe(s: &mut Session, uid: u64) -> Seen {
    let seen: Seen = Arc::default();
    let sink = seen.clone();
    s.set_edit_observer(uid, Some(Box::new(move |_| *sink.lock().expect("lock") += 1)));
    seen
}

fn reports(seen: &Seen) -> usize {
    *seen.lock().expect("lock")
}

fn uid(s: &Session) -> u64 {
    s.active().expect("a document").uid
}

fn doc(s: &Session) -> Arc<Presentation> {
    s.active().expect("a document").doc.clone()
}

fn insert(s: &mut Session, x: f64, y: f64) -> u64 {
    let r = s.execute("shape.insert", &json!({"preset": "rect", "rect": [x, y, 80.0, 60.0]})).expect("insert");
    r["id"].as_u64().expect("an id")
}

fn ev(kind: PointerKind, x: f64, y: f64) -> PointerEvent {
    PointerEvent { kind, x, y, mods: Mods::default(), tol: 4.0, pressure: None }
}

/// A new presentation with `n` slides, on slide `at` with a shape selected there.
fn deck(n: usize, at: usize) -> (Session, u64) {
    let mut s = Session::with_new();
    while doc(&s).slides.len() < n {
        s.execute("slide.new", &json!({})).expect("slide");
    }
    s.execute("slide.go", &json!({"index": at})).expect("go");
    let id = insert(&mut s, 100.0, 100.0);
    s.execute("edit.select", &json!({"ids": [id]})).expect("select");
    (s, id)
}

fn current_slide_id(s: &Session) -> SlideId {
    s.active().and_then(|d| d.current_slide()).expect("a current slide").id
}

/// The slide list as someone else changed it: `f` edits a copy of the ids' order.
fn reordered(p: &Presentation, f: impl FnOnce(&mut Vec<Arc<Slide>>)) -> Vec<Arc<Slide>> {
    let mut v = p.slides.clone();
    f(&mut v);
    v
}

#[test]
fn apply_remote_adds_no_undo_step_and_is_not_observed() {
    let mut s = Session::with_new();
    s.execute("slide.new", &json!({})).expect("slide");
    let local = insert(&mut s, 50.0, 50.0);
    let u = uid(&s);
    let seen = observe(&mut s, u);
    let (undo, redo, journal) = {
        let st = s.active().expect("doc");
        (st.history.undo.len(), st.history.redo.len(), s.journal.len())
    };
    let before = doc(&s);
    s.apply_remote(u, |ctx| {
        let slides = reordered(ctx.presentation(), |v| v.reverse());
        ctx.set_slides(slides);
        Ok(())
    })
    .expect("remote");
    let after = doc(&s);
    assert!(!Arc::ptr_eq(&before, &after), "the remote change landed");
    let st = s.active().expect("doc");
    assert_eq!((st.history.undo.len(), st.history.redo.len()), (undo, redo), "no undo step");
    assert_eq!(s.journal.len(), journal, "no journal entry");
    assert_eq!(reports(&seen), 0, "not reported");

    // Undo still undoes the earlier local step.
    s.execute("edit.undo", &json!({})).expect("undo");
    assert!(doc(&s).slides.iter().all(|sl| sl.shapes.iter().all(|sh| sh.id.0 != local as u32)), "the local insert is undone");
}

#[test]
fn apply_remote_keeps_the_selection_by_id() {
    let (mut s, shape) = deck(4, 1);
    let u = uid(&s);
    let (slide, index) = (current_slide_id(&s), s.active().expect("doc").selection.slide);
    s.apply_remote(u, |ctx| {
        let slides = reordered(ctx.presentation(), |v| v.reverse());
        ctx.set_slides(slides);
        Ok(())
    })
    .expect("remote");
    let st = s.active().expect("doc");
    assert_ne!(st.selection.slide, index, "the current slide moved");
    assert_eq!(current_slide_id(&s), slide, "the same slide is current");
    assert_eq!(st.selection.shapes, vec![ShapeId(shape as u32)], "the same shape is selected");
}

#[test]
fn a_removed_current_slide_moves_the_selection_to_a_neighbour() {
    let (mut s, _) = deck(4, 1);
    let u = uid(&s);
    let (gone, next) = {
        let p = doc(&s);
        (p.slides[1].id, p.slides[2].id)
    };
    s.apply_remote(u, |ctx| {
        let slides = reordered(ctx.presentation(), |v| v.retain(|sl| sl.id != gone));
        ctx.set_slides(slides);
        Ok(())
    })
    .expect("remote");
    assert_eq!(current_slide_id(&s), next, "the slide now at its index");
    let st = s.active().expect("doc");
    assert!(st.selection.shapes.is_empty() && st.selection.text.is_none(), "nothing of the gone slide stays selected");

    // The last slide removed: the one before it, now the last, is current.
    let (mut s, _) = deck(3, 2);
    let u = uid(&s);
    let (gone, before) = {
        let p = doc(&s);
        (p.slides[2].id, p.slides[1].id)
    };
    s.apply_remote(u, |ctx| {
        let slides = reordered(ctx.presentation(), |v| v.retain(|sl| sl.id != gone));
        ctx.set_slides(slides);
        Ok(())
    })
    .expect("remote");
    assert_eq!(current_slide_id(&s), before);
}

/// Every connector end and animation on the current slide that names `id`.
fn mentions(p: &Presentation, slide: SlideId, id: ShapeId) -> usize {
    let Some(sl) = p.slide(slide) else { return 0 };
    let mut n = sl.animations.iter().filter(|a| a.shape == id || a.trigger == Some(id)).count();
    deckcraft_engine::model::walk(&sl.shapes, &mut |sh, _| {
        if let ShapeKind::Connector { start, end } = &sh.kind {
            n += [start, end].iter().filter(|e| e.is_some_and(|(to, _)| to == id)).count();
        }
    });
    n
}

#[test]
fn remote_delete_equals_local_delete() {
    let mut a = Session::with_new();
    let target = insert(&mut a, 100.0, 100.0);
    let other = insert(&mut a, 400.0, 300.0);
    let partner = insert(&mut a, 100.0, 300.0);
    a.execute("shape.connect", &json!({"from": target, "to": other})).expect("connect");
    a.execute("animation.add", &json!({"effect": "fade", "ids": [target]})).expect("animate");
    a.execute("animation.add", &json!({"effect": "spin", "ids": [other]})).expect("animate other");
    a.execute("animation.timing", &json!({"index": 1, "trigger": target})).expect("triggered by the target");
    // In a group with a partner, so the group refits once the target is gone.
    a.execute("arrange.group", &json!({"ids": [target, partner]})).expect("group");
    a.execute("edit.deselect", &json!({})).expect("deselect");
    let slide = current_slide_id(&a);
    let id = ShapeId(target as u32);
    assert!(mentions(&doc(&a), slide, id) >= 3, "the target is a connector end, animated and a trigger");

    let mut b = Session::new();
    b.add_document(a.active().expect("doc").clone());
    let ub = uid(&b);

    a.execute("edit.delete", &json!({"ids": [target]})).expect("local delete");
    let gone = b.apply_remote(ub, |ctx| ctx.delete_shapes(slide, &[id])).expect("remote delete");
    assert!(gone.contains(&id), "the target is reported gone");
    let (pa, pb) = (doc(&a), doc(&b));
    assert_eq!(mentions(&pa, slide, id), 0, "nothing names the deleted shape");
    assert!(*pa == *pb, "a remote delete leaves the deck as a local delete does");
}

#[test]
fn remote_change_waits_while_a_drag_is_open() {
    let mut s = Session::with_new();
    insert(&mut s, 100.0, 100.0);
    s.execute("edit.deselect", &json!({})).expect("deselect");
    let u = uid(&s);
    let remote = |s: &mut Session| {
        s.apply_remote(u, |ctx| {
            let slides = reordered(ctx.presentation(), |v| {
                let mut sl = (*v[0]).clone();
                sl.name = "renamed by someone else".into();
                v[0] = Arc::new(sl);
            });
            ctx.set_slides(slides);
            Ok(())
        })
    };
    s.pointer(ev(PointerKind::Down, 140.0, 130.0)).expect("down");
    s.pointer(ev(PointerKind::Drag, 200.0, 160.0)).expect("drag");
    assert!(s.remote_busy(u));
    let during = doc(&s);
    assert!(matches!(remote(&mut s), Err(EngineError::RemoteBusy)));
    assert!(Arc::ptr_eq(&during, &doc(&s)), "the document is unchanged");
    s.pointer(ev(PointerKind::Up, 200.0, 160.0)).expect("up");
    assert!(!s.remote_busy(u));
    let dropped = doc(&s);
    remote(&mut s).expect("after the drop");
    assert!(!Arc::ptr_eq(&dropped, &doc(&s)), "the change landed after the drop");
}

#[test]
fn a_failed_remote_change_leaves_the_document_as_before() {
    let (mut s, _) = deck(3, 1);
    let u = uid(&s);
    let (before, sel) = (doc(&s), s.active().expect("doc").selection.clone());
    let r: Result<(), _> = s.apply_remote(u, |ctx| {
        let slides = reordered(ctx.presentation(), |v| v.reverse());
        ctx.set_slides(slides);
        Err(EngineError::Other("the replica could not be read".into()))
    });
    assert!(r.is_err());
    assert!(s.apply_remote(u, |ctx| ctx.delete_shapes(SlideId(u32::MAX), &[ShapeId(1)])).is_err(), "no such slide");
    assert!(Arc::ptr_eq(&before, &doc(&s)), "the same presentation");
    assert_eq!(s.active().expect("doc").selection, sel, "the same selection");
}

#[test]
fn a_quiet_replace_keeps_the_selection_and_adds_no_undo_step() {
    let (mut s, shape) = deck(3, 1);
    let u = uid(&s);
    let seen = observe(&mut s, u);
    let slide = current_slide_id(&s);
    let undo = s.active().expect("doc").history.undo.len();
    let mut p = (*doc(&s)).clone();
    p.slides.rotate_left(1);
    s.replace_presentation_quiet(u, p).expect("reset");
    let st = s.active().expect("doc");
    assert_eq!(current_slide_id(&s), slide);
    assert_eq!(st.selection.shapes, vec![ShapeId(shape as u32)]);
    assert_eq!(st.history.undo.len(), undo);
    assert_eq!(reports(&seen), 0);
}

#[test]
fn live_undo_hook_takes_undo_and_redo_for_its_document_only() {
    let mut s = Session::with_new();
    let live = uid(&s);
    insert(&mut s, 100.0, 100.0);
    let calls: Arc<Mutex<Vec<bool>>> = Arc::default();
    let sink = calls.clone();
    s.set_live_undo_hook(
        live,
        Some(Box::new(move |redo| {
            sink.lock().expect("lock").push(redo);
            true
        })),
    );
    let seen = observe(&mut s, live);
    let (before, undo, redo) = {
        let st = s.active().expect("doc");
        (st.doc.clone(), st.history.undo.len(), st.history.redo.len())
    };
    s.execute("edit.undo", &json!({})).expect("undo");
    s.execute("edit.redo", &json!({})).expect("redo");
    assert_eq!(*calls.lock().expect("lock"), vec![false, true], "the hook took Undo, then Redo");
    let st = s.active().expect("doc");
    assert!(Arc::ptr_eq(&before, &st.doc), "the snapshots were not restored");
    assert_eq!((st.history.undo.len(), st.history.redo.len()), (undo, redo), "the history is unchanged");
    assert_eq!(reports(&seen), 0, "not reported as a local edit");

    // A second document still undoes with its snapshots.
    s.execute("file.new", &json!({})).expect("second");
    assert_ne!(uid(&s), live);
    let empty = doc(&s);
    insert(&mut s, 10.0, 10.0);
    s.execute("edit.undo", &json!({})).expect("undo");
    assert!(*doc(&s) == *empty, "the snapshot was restored");
    assert_eq!(calls.lock().expect("lock").len(), 2, "the hook heard nothing of it");
}

/// Someone else renames the first slide.
fn rename_first_slide(s: &mut Session, u: u64) -> deckcraft_engine::Result<()> {
    s.apply_remote(u, |ctx| {
        let slides = reordered(ctx.presentation(), |v| {
            let mut sl = (*v[0]).clone();
            sl.name = format!("{} (theirs)", sl.name);
            v[0] = Arc::new(sl);
        });
        ctx.set_slides(slides);
        Ok(())
    })
}

#[test]
fn a_remote_change_never_lands_inside_a_drop_or_commit_report() {
    // A drag: the remote change is held, the drop reports only the move.
    let mut s = Session::with_new();
    insert(&mut s, 100.0, 100.0);
    s.execute("edit.deselect", &json!({})).expect("deselect");
    let u = uid(&s);
    let got: Arc<Mutex<Vec<deckcraft_engine::Edited>>> = Arc::default();
    let sink = got.clone();
    s.set_edit_observer(u, Some(Box::new(move |e| sink.lock().expect("lock").push(e))));
    let pressed = doc(&s);
    s.pointer(ev(PointerKind::Down, 140.0, 130.0)).expect("down");
    s.pointer(ev(PointerKind::Drag, 200.0, 160.0)).expect("drag");
    assert!(matches!(rename_first_slide(&mut s, u), Err(EngineError::RemoteBusy)));
    s.pointer(ev(PointerKind::Up, 200.0, 160.0)).expect("up");
    let dropped = doc(&s);
    rename_first_slide(&mut s, u).expect("after the drop");
    {
        let got = got.lock().expect("lock");
        assert_eq!(got.len(), 1, "only the drop is reported");
        assert!(Arc::ptr_eq(&got[0].before, &pressed) && Arc::ptr_eq(&got[0].after, &dropped));
        assert!(got[0].after.slides[0].name == pressed.slides[0].name, "the drop holds none of the remote change");
    }
    got.lock().expect("lock").clear();

    // A composition: likewise, its commit reports only the composed text.
    s.execute("insert.textBox", &json!({"rect": [300.0, 300.0, 200.0, 50.0]})).expect("text box");
    got.lock().expect("lock").clear();
    let before = doc(&s);
    s.compose_text("ka", None).expect("compose");
    assert!(s.remote_busy(u));
    assert!(matches!(rename_first_slide(&mut s, u), Err(EngineError::RemoteBusy)));
    s.commit_text("か").expect("commit");
    let committed = doc(&s);
    rename_first_slide(&mut s, u).expect("after the commit");
    let got = got.lock().expect("lock");
    assert_eq!(got.len(), 1, "only the commit is reported");
    assert!(Arc::ptr_eq(&got[0].before, &before) && Arc::ptr_eq(&got[0].after, &committed));
    assert!(got[0].after.slides[0].name == before.slides[0].name, "the commit holds none of the remote change");
}

/// The slide list with shape `id` on the current slide changed by `f`, as someone else changed it.
fn with_shape(p: &Presentation, slide: SlideId, id: ShapeId, f: impl FnOnce(&mut deckcraft_engine::model::Shape)) -> Vec<Arc<Slide>> {
    reordered(p, |v| {
        if let Some(i) = v.iter().position(|s| s.id == slide) {
            let mut sl = (*v[i]).clone();
            if let Some(sh) = deckcraft_engine::model::find_shape_mut(&mut sl.shapes, id) {
                f(sh);
            }
            v[i] = Arc::new(sl);
        }
    })
}

/// D4 review M1: a connector inside a group glued to the deleted shape loses that end, the same
/// way for a local and a remote delete.
#[test]
fn a_connector_inside_a_group_loses_its_end_alike_for_a_local_and_a_remote_delete() {
    let mut a = Session::with_new();
    let target = insert(&mut a, 100.0, 100.0);
    let partner = insert(&mut a, 400.0, 300.0);
    let line = a.execute("shape.connect", &json!({"from": partner, "to": target})).expect("connect")["id"].as_u64().expect("connector id");
    a.execute("arrange.group", &json!({"ids": [line, partner]})).expect("group");
    a.execute("edit.deselect", &json!({})).expect("deselect");
    let slide = current_slide_id(&a);
    let (id, line) = (ShapeId(target as u32), ShapeId(line as u32));
    let in_group =
        |p: &Presentation| p.slide(slide).is_some_and(|s| s.shapes.iter().any(|g| g.is_group() && g.children().iter().any(|x| x.id == line)));
    assert!(in_group(&doc(&a)), "the connector sits inside the group");
    assert!(mentions(&doc(&a), slide, id) >= 1, "the connector is glued to the target");

    let mut b = Session::new();
    b.add_document(a.active().expect("doc").clone());
    let ub = uid(&b);
    a.execute("edit.delete", &json!({"ids": [target]})).expect("local delete");
    b.apply_remote(ub, |ctx| ctx.delete_shapes(slide, &[id]).map(|_| ())).expect("remote delete");
    let (pa, pb) = (doc(&a), doc(&b));
    assert_eq!(mentions(&pa, slide, id), 0, "the connector no longer names the deleted shape");
    assert!(pa.slide(slide).is_some_and(|s| s.shape(line).is_some()), "the connector itself stays");
    assert!(*pa == *pb, "a remote delete leaves the deck as a local delete does");
}

/// D4 review M4: a press with no drag yet already holds a remote change off.
#[test]
fn a_press_alone_holds_a_remote_change() {
    let mut s = Session::with_new();
    insert(&mut s, 100.0, 100.0);
    s.execute("edit.deselect", &json!({})).expect("deselect");
    let u = uid(&s);
    s.pointer(ev(PointerKind::Down, 140.0, 130.0)).expect("down");
    assert!(s.remote_busy(u), "a press is a gesture in progress");
    let during = doc(&s);
    assert!(matches!(rename_first_slide(&mut s, u), Err(EngineError::RemoteBusy)));
    assert!(Arc::ptr_eq(&during, &doc(&s)), "the document is unchanged");
    s.pointer(ev(PointerKind::Up, 140.0, 130.0)).expect("up");
    assert!(!s.remote_busy(u));
    rename_first_slide(&mut s, u).expect("after the release");
}

/// D4 review M4: a reset waits for a composition as a remote change does.
#[test]
fn a_quiet_replace_is_refused_during_a_composition() {
    let mut s = Session::with_new();
    let id = insert(&mut s, 100.0, 100.0);
    let u = uid(&s);
    s.execute("text.edit", &json!({"id": id})).expect("edit");
    s.compose_text("k", None).expect("compose");
    assert!(s.remote_busy(u));
    let during = doc(&s);
    let mut p = (*during).clone();
    p.slides.reverse();
    assert!(matches!(s.replace_presentation_quiet(u, p), Err(EngineError::RemoteBusy)));
    assert!(Arc::ptr_eq(&during, &doc(&s)), "the document is unchanged");
}

/// D4 review M4: a remote change that shortens the text being edited keeps the caret inside it,
/// and a caret the live layer places wins over that clamp.
#[test]
fn a_shortened_text_clamps_the_caret_and_set_text_wins() {
    let mut s = Session::with_new();
    let id = insert(&mut s, 100.0, 100.0);
    let shape = ShapeId(id as u32);
    let u = uid(&s);
    s.execute("text.edit", &json!({"id": id})).expect("edit");
    s.execute("text.insert", &json!({"text": "Hello world"})).expect("type");
    let slide = current_slide_id(&s);
    let caret = |s: &Session| s.active().and_then(|d| d.selection.text.clone()).expect("a caret");
    assert!(caret(&s).caret.1 > 2, "the caret is past what will be left");
    s.apply_remote(u, |ctx| {
        let slides = with_shape(ctx.presentation(), slide, shape, |sh| sh.text = Some(deckcraft_engine::model::text::TextBody::from_text("Hi")));
        ctx.set_slides(slides);
        Ok(())
    })
    .expect("shorten");
    let c = caret(&s);
    assert!(c.caret.1 <= 2 && c.anchor.1 <= 2, "the caret is kept inside the shorter text");
    s.apply_remote(u, |ctx| {
        let slides = with_shape(ctx.presentation(), slide, shape, |sh| sh.text = Some(deckcraft_engine::model::text::TextBody::from_text("Hey")));
        ctx.set_slides(slides);
        ctx.set_text(Some(deckcraft_engine::TextSel { anchor: (0, 1), caret: (0, 1), ..c.clone() }));
        Ok(())
    })
    .expect("placed");
    assert!(caret(&s).caret == (0, 1), "the placed caret wins");
}

/// D4 review M4: a panic inside a remote change is caught and the document is left as it was.
#[test]
fn a_panicking_remote_change_leaves_the_document_as_before() {
    let (mut s, _) = deck(3, 1);
    let u = uid(&s);
    let before = doc(&s);
    let r: Result<(), _> = s.apply_remote(u, |ctx| {
        let slides = reordered(ctx.presentation(), |v| v.reverse());
        ctx.set_slides(slides);
        panic!("the replica was unreadable");
    });
    assert!(r.is_err(), "the panic is an error");
    assert!(Arc::ptr_eq(&before, &doc(&s)), "the same presentation");
}

/// D4 review M2: a remote change that takes rows and columns keeps a cell caret and a selected
/// cell range inside the table.
#[test]
fn a_smaller_table_clamps_the_cell_caret_and_range() {
    let mut s = Session::with_new();
    let id = s.execute("insert.table", &json!({"rows": 3, "cols": 3})).expect("table")["id"].as_u64().expect("table id");
    let table = ShapeId(id as u32);
    let u = uid(&s);
    s.execute("text.edit", &json!({"id": id, "cell": [2, 2]})).expect("edit the last cell");
    s.execute("text.insert", &json!({"text": "corner"})).expect("type");
    let slide = current_slide_id(&s);
    s.apply_remote(u, |ctx| {
        let slides = with_shape(ctx.presentation(), slide, table, |sh| {
            if let ShapeKind::Table(t) = &mut sh.kind {
                t.delete_row(2);
                t.delete_col(2);
            }
        });
        ctx.set_slides(slides);
        Ok(())
    })
    .expect("shrink");
    let st = s.active().expect("doc");
    let t = st.selection.text.clone().expect("still editing");
    let (r, c) = t.cell.expect("in a cell");
    assert!(r < 2 && c < 2, "the cell caret is inside the smaller table");
    if let Some((_, a, b)) = st.selection.cells {
        assert!(a.0 < 2 && a.1 < 2 && b.0 < 2 && b.1 < 2, "the selected range is inside the smaller table");
    }
}

/// D4 review M2: a selected cell range is kept inside a table that lost rows and columns.
#[test]
fn a_smaller_table_clamps_the_selected_cells() {
    let mut s = Session::with_new();
    let id = s.execute("insert.table", &json!({"rows": 3, "cols": 3})).expect("table")["id"].as_u64().expect("table id");
    let table = ShapeId(id as u32);
    let u = uid(&s);
    s.execute("edit.select", &json!({"ids": [id]})).expect("select");
    s.execute("table.selectCells", &json!({"from": [0, 0], "to": [2, 2]})).expect("select cells");
    assert!(s.active().and_then(|d| d.selection.cells).is_some(), "a cell range is selected");
    let slide = current_slide_id(&s);
    s.apply_remote(u, |ctx| {
        let slides = with_shape(ctx.presentation(), slide, table, |sh| {
            if let ShapeKind::Table(t) = &mut sh.kind {
                t.delete_row(2);
                t.delete_col(2);
            }
        });
        ctx.set_slides(slides);
        Ok(())
    })
    .expect("shrink");
    let (_, a, b) = s.active().and_then(|d| d.selection.cells).expect("the range stays");
    assert!(a.0 < 2 && a.1 < 2 && b.0 < 2 && b.1 < 2, "the selected range is inside the smaller table");
}
