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
