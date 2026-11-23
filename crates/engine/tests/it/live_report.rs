//! The edit observer (H1): every change to an observed presentation is reported once, at the end
//! of the command, drop or composition that made it, with the presentations around it.

use std::sync::{Arc, Mutex};

use deckcraft_engine::model::Presentation;
use deckcraft_engine::{Edited, Mods, PointerEvent, PointerKind, Session, ToolKind, command_specs, sample};
use serde_json::{Value, json};

type Seen = Arc<Mutex<Vec<Edited>>>;

fn observe(s: &mut Session, uid: u64) -> Seen {
    let seen: Seen = Arc::default();
    let sink = seen.clone();
    s.set_edit_observer(uid, Some(Box::new(move |e| sink.lock().expect("lock").push(e))));
    seen
}

fn take(seen: &Seen) -> Vec<Edited> {
    std::mem::take(&mut *seen.lock().expect("lock"))
}

fn doc_of(s: &Session, uid: u64) -> Option<Arc<Presentation>> {
    s.documents().iter().find(|d| d.uid == uid).map(|d| d.doc.clone())
}

fn uid(s: &Session) -> u64 {
    s.active().expect("a document").uid
}

/// Exactly one report, for `uid`, from `before` to `after` (the same allocations).
fn assert_one(got: &[Edited], uid: u64, before: &Arc<Presentation>, after: &Arc<Presentation>, what: &str) {
    assert_eq!(got.len(), 1, "{what}: one report, got {:?}", got.iter().map(|e| &e.command).collect::<Vec<_>>());
    let e = &got[0];
    assert_eq!(e.doc, uid, "{what}: the observed document");
    assert!(Arc::ptr_eq(&e.before, before), "{what}: `before` is the presentation before");
    assert!(Arc::ptr_eq(&e.after, after), "{what}: `after` is the presentation after");
}

fn ev(kind: PointerKind, x: f64, y: f64) -> PointerEvent {
    PointerEvent { kind, x, y, mods: Mods::default(), tol: 4.0, pressure: None }
}

fn ink(mode: &str) -> ToolKind {
    ToolKind::Ink { mode: mode.into(), color: deckcraft_engine::model::Rgba::rgb(0, 0, 0), width: 2.0 }
}

/// The sample deck on its second slide, that slide selected, its body shape selected and a caret
/// in the body's text.
fn editing_sample() -> Session {
    let mut s = Session::new();
    sample::open_sample(&mut s).expect("sample");
    s.execute("slide.go", &json!({"index": 1})).expect("go");
    let st = s.active().expect("doc");
    let slide = st.current_slide().expect("slide");
    let body = slide.shapes.iter().find(|x| x.text.is_some() && x.id != slide.shapes[0].id).map(|x| x.id.0).expect("a body");
    let sid = slide.id.0;
    s.execute("slide.selectSlides", &json!({"ids": [sid]})).expect("slide selected");
    s.execute("edit.select", &json!({"ids": [body]})).expect("select");
    s.execute("text.edit", &json!({"id": body, "at": [0, 3]})).expect("caret");
    s
}

/// Commands that read or write files outside the session.
fn touches_files(id: &str) -> bool {
    matches!(id, "file.open" | "file.save" | "file.saveAs" | "file.export" | "file.saveTemplate")
}

#[test]
fn every_registered_command_change_is_reported() {
    let template = editing_sample();
    let st = template.active().expect("doc").clone();
    assert!(st.selection.text.is_some() && !st.selection.shapes.is_empty(), "a caret and a selected shape");
    let mut changed = 0;
    for spec in command_specs().iter().filter(|c| !touches_files(c.id)) {
        let mut s = Session::new();
        s.prefs = template.prefs.clone();
        s.add_document(st.clone());
        let uid = st.uid;
        let seen = observe(&mut s, uid);
        let before = doc_of(&s, uid).expect("open");
        let steps = undo_len(&s, uid);
        let _ = s.execute(spec.id, &json!({}));
        let got = take(&seen);
        match doc_of(&s, uid) {
            Some(after) if !Arc::ptr_eq(&before, &after) => {
                changed += 1;
                assert_one(&got, uid, &before, &after, spec.id);
                assert_eq!(got[0].command, spec.id);
            }
            _ => assert!(got.is_empty(), "{}: nothing changed, nothing reported, got {}", spec.id, got.len()),
        }
        // Every undo step a command made is a change that was reported.
        if undo_len(&s, uid) > steps {
            assert_eq!(got.len(), 1, "{}: an undo step without a report", spec.id);
        }
    }
    assert!(changed > 0, "some command changed the deck");
}

fn undo_len(s: &Session, uid: u64) -> usize {
    s.documents().iter().find(|d| d.uid == uid).map_or(0, |d| d.history.undo.len())
}

/// Each report's `before` is the previous one's `after`.
fn assert_chained(got: &[Edited]) {
    for w in got.windows(2) {
        assert!(Arc::ptr_eq(&w[0].after, &w[1].before), "{} then {}: the reports chain", w[0].command, w[1].command);
    }
}

#[test]
fn undo_and_redo_with_real_history_report_once_each_and_chain() {
    let mut s = Session::with_new();
    let uid = uid(&s);
    let seen = observe(&mut s, uid);
    let empty = doc_of(&s, uid).expect("open");
    s.execute("shape.insert", &json!({"preset": "rect", "rect": [10.0, 10.0, 50.0, 50.0]})).expect("insert");
    let inserted = doc_of(&s, uid).expect("open");
    s.execute("edit.undo", &json!({})).expect("undo");
    let undone = doc_of(&s, uid).expect("open");
    s.execute("edit.redo", &json!({})).expect("redo");
    let redone = doc_of(&s, uid).expect("open");
    let got = take(&seen);
    assert_eq!(got.iter().map(|e| e.command.as_str()).collect::<Vec<_>>(), ["shape.insert", "edit.undo", "edit.redo"]);
    assert!(Arc::ptr_eq(&got[0].before, &empty));
    assert!(Arc::ptr_eq(&got[1].before, &inserted) && Arc::ptr_eq(&got[1].after, &undone));
    assert!(Arc::ptr_eq(&got[2].after, &redone));
    assert_chained(&got);
}

#[test]
fn delete_text_reports_once() {
    let mut s = editing_sample();
    let uid = uid(&s);
    let seen = observe(&mut s, uid);
    let (before, steps) = (doc_of(&s, uid).expect("open"), undo_len(&s, uid));
    let r = s.execute("text.deleteSurrounding", &json!({"before": 1, "after": 1})).expect("delete");
    assert!(r["deleted"].as_u64().is_some_and(|n| n > 0), "text was deleted: {r}");
    let after = doc_of(&s, uid).expect("open");
    assert_one(&take(&seen), uid, &before, &after, "Delete Text");
    assert!(undo_len(&s, uid) > steps, "one undo step");
}

/// A new presentation with one rectangle selected.
fn one_selected_shape() -> (Session, u64) {
    let mut s = Session::with_new();
    let id = s.execute("shape.insert", &json!({"preset": "rect", "rect": [100.0, 100.0, 100.0, 60.0]})).expect("insert")["id"].as_u64().expect("id");
    s.execute("edit.select", &json!({"ids": [id]})).expect("select");
    (s, id)
}

#[test]
fn a_twist_reports_once_at_its_end() {
    let (mut s, _) = one_selected_shape();
    let uid = uid(&s);
    let seen = observe(&mut s, uid);
    let before = doc_of(&s, uid).expect("open");
    for _ in 0..4 {
        s.execute("shape.rotate", &json!({"by": 5.0, "gesture": "move"})).expect("twist");
        assert!(take(&seen).is_empty(), "nothing during the twist");
    }
    s.execute("shape.rotate", &json!({"gesture": "end"})).expect("end");
    let after = doc_of(&s, uid).expect("open");
    assert!(!Arc::ptr_eq(&before, &after), "the shape turned");
    assert_one(&take(&seen), uid, &before, &after, "twist");
}

#[test]
fn a_twist_whose_end_never_came_is_settled_by_the_next_command() {
    let (mut s, _) = one_selected_shape();
    let uid = uid(&s);
    let seen = observe(&mut s, uid);
    let (before, steps) = (doc_of(&s, uid).expect("open"), undo_len(&s, uid));
    for _ in 0..3 {
        s.execute("shape.rotate", &json!({"by": 5.0, "gesture": "move"})).expect("twist");
    }
    let twisted = doc_of(&s, uid).expect("open");
    // The host lost `gesture: end`; the next command must not be folded into the twist.
    s.execute("slide.new", &json!({})).expect("slide");
    let got = take(&seen);
    assert_eq!(got.len(), 2, "{:?}", got.iter().map(|e| &e.command).collect::<Vec<_>>());
    assert!(Arc::ptr_eq(&got[0].before, &before) && Arc::ptr_eq(&got[0].after, &twisted), "the twist alone");
    assert_eq!(got[1].command, "slide.new");
    assert_chained(&got);
    assert_eq!(undo_len(&s, uid), steps + 2, "the twist and the new slide are two undo steps");
    assert!(!s.remote_busy(uid), "nothing holds remote changes any more");
}

#[test]
fn a_pointer_gesture_that_runs_a_command_reports_once() {
    let mut s = Session::with_new();
    s.execute("edit.deselect", &json!({})).expect("deselect");
    let uid = uid(&s);
    let seen = observe(&mut s, uid);
    let before = doc_of(&s, uid).expect("open");
    s.set_tool(ToolKind::Shape { preset: "rect".into() });
    s.pointer(ev(PointerKind::Down, 200.0, 200.0)).expect("down");
    s.pointer(ev(PointerKind::Up, 200.0, 200.0)).expect("up");
    let after = doc_of(&s, uid).expect("open");
    assert!(!Arc::ptr_eq(&before, &after), "a shape was inserted");
    assert_one(&take(&seen), uid, &before, &after, "click to insert");
}

#[test]
fn a_drag_reports_once_at_the_drop() {
    let mut s = Session::with_new();
    s.execute("shape.insert", &json!({"preset": "rect", "rect": [100.0, 100.0, 100.0, 100.0]})).expect("insert");
    s.execute("edit.deselect", &json!({})).expect("deselect");
    let uid = uid(&s);
    let seen = observe(&mut s, uid);
    let at_press = doc_of(&s, uid).expect("open");
    s.pointer(ev(PointerKind::Down, 150.0, 150.0)).expect("down");
    for i in 1..=10 {
        s.pointer(ev(PointerKind::Drag, 150.0 + 10.0 * f64::from(i), 150.0 + 5.0 * f64::from(i))).expect("drag");
        assert!(take(&seen).is_empty(), "nothing during the drag (move {i})");
    }
    let mid = doc_of(&s, uid).expect("open");
    assert!(!Arc::ptr_eq(&mid, &at_press), "the drag moved the shape");
    s.pointer(ev(PointerKind::Up, 250.0, 200.0)).expect("up");
    let after = doc_of(&s, uid).expect("open");
    let got = take(&seen);
    assert_one(&got, uid, &at_press, &after, "drop");
    assert!(got[0].command.starts_with("pointer:"), "{}", got[0].command);
}

#[test]
fn an_ime_composition_reports_once_at_commit() {
    let mut s = editing_sample();
    let uid = uid(&s);
    let seen = observe(&mut s, uid);
    let before = doc_of(&s, uid).expect("open");
    for pre in ["k", "ka", "kan", "かん"] {
        s.compose_text(pre, None).expect("compose");
        assert!(take(&seen).is_empty(), "nothing while composing `{pre}`");
    }
    s.commit_text("漢").expect("commit");
    let after = doc_of(&s, uid).expect("open");
    let got = take(&seen);
    assert_one(&got, uid, &before, &after, "commit");
    assert_eq!(got[0].command, "text.compose");

    // A composition the next command settles is its own report, then the command's.
    s.compose_text("x", None).expect("compose");
    let composed = doc_of(&s, uid).expect("open");
    s.execute("text.insert", &json!({"text": "y"})).expect("type");
    let got = take(&seen);
    assert_eq!(got.len(), 2, "{:?}", got.iter().map(|e| &e.command).collect::<Vec<_>>());
    assert!(Arc::ptr_eq(&got[0].before, &after) && Arc::ptr_eq(&got[0].after, &composed));
    assert_eq!(got[0].command, "text.compose");
    assert!(Arc::ptr_eq(&got[1].before, &composed) && Arc::ptr_eq(&got[1].after, &doc_of(&s, uid).expect("open")));
    assert_eq!(got[1].command, "text.insert");

    // A cancelled composition changed nothing.
    s.compose_text("z", None).expect("compose");
    s.compose_text("", None).expect("cancel");
    assert!(take(&seen).is_empty());
}

#[test]
fn a_nested_command_reports_only_the_outer_one() {
    let mut s = Session::with_new();
    s.execute("edit.deselect", &json!({})).expect("deselect");
    let uid = uid(&s);
    let seen = observe(&mut s, uid);
    let before = doc_of(&s, uid).expect("open");
    // With no text being edited, Paste and Match Formatting runs `insert.textBox` inside it.
    let r = s.execute("edit.pasteText", &json!({"text": "hello"})).expect("paste");
    assert!(r.get("id").and_then(Value::as_u64).is_some(), "a text box was inserted: {r}");
    let after = doc_of(&s, uid).expect("open");
    let got = take(&seen);
    assert_one(&got, uid, &before, &after, "paste");
    assert_eq!(got[0].command, "edit.pasteText");
}

#[test]
fn observer_is_per_document() {
    let mut s = Session::with_new();
    let first = uid(&s);
    let seen = observe(&mut s, first);
    s.execute("file.new", &json!({})).expect("second");
    let second = uid(&s);
    assert_ne!(first, second);
    s.execute("shape.insert", &json!({"preset": "rect", "rect": [10.0, 10.0, 50.0, 50.0]})).expect("insert");
    s.execute("slide.new", &json!({})).expect("slide");
    assert!(take(&seen).is_empty(), "edits in the second document are not the first one's");

    s.set_active(0);
    let before = doc_of(&s, first).expect("open");
    s.execute("slide.new", &json!({})).expect("slide");
    assert_one(&take(&seen), first, &before, &doc_of(&s, first).expect("open"), "first document");

    // Replaced, then removed: the old observer hears nothing more.
    let other = observe(&mut s, first);
    s.execute("slide.new", &json!({})).expect("slide");
    assert!(take(&seen).is_empty());
    assert_eq!(take(&other).len(), 1);
    s.set_edit_observer(first, None);
    s.execute("slide.new", &json!({})).expect("slide");
    assert!(take(&other).is_empty());
}

#[test]
fn a_failed_command_reports_nothing() {
    let mut s = editing_sample();
    let uid = uid(&s);
    let seen = observe(&mut s, uid);
    let before = doc_of(&s, uid).expect("open");
    assert!(s.execute("shape.fill", &json!({"ids": [999_999], "color": "#ff0000"})).is_err());
    assert!(s.execute("slide.delete", &json!({"index": 99_999})).is_err());
    assert!(s.execute("no.such.command", &json!({})).is_err());
    assert!(Arc::ptr_eq(&before, &doc_of(&s, uid).expect("open")), "nothing changed");
    assert!(take(&seen).is_empty());
}

#[test]
fn an_erase_reports_once() {
    let mut s = Session::with_new();
    s.execute("edit.deselect", &json!({})).expect("deselect");
    let uid = uid(&s);
    let seen = observe(&mut s, uid);
    let before = doc_of(&s, uid).expect("open");
    s.set_tool(ink("pen"));
    s.pointer(ev(PointerKind::Down, 100.0, 100.0)).expect("down");
    for i in 1..=5 {
        s.pointer(ev(PointerKind::Drag, 100.0 + 10.0 * f64::from(i), 100.0)).expect("drag");
    }
    s.pointer(ev(PointerKind::Up, 150.0, 100.0)).expect("up");
    let inked = doc_of(&s, uid).expect("open");
    let got = take(&seen);
    assert_one(&got, uid, &before, &inked, "ink stroke");

    s.set_tool(ink("eraser"));
    s.pointer(ev(PointerKind::Down, 120.0, 100.0)).expect("erase");
    s.pointer(ev(PointerKind::Up, 120.0, 100.0)).expect("up");
    let erased = doc_of(&s, uid).expect("open");
    assert!(!Arc::ptr_eq(&inked, &erased), "the stroke was erased");
    let got = take(&seen);
    assert_one(&got, uid, &inked, &erased, "erase");
    assert_eq!(got[0].command, "pointer:Erase");
}
