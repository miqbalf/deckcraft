//! Copy, cut and paste by keyboard in a browser (DP1). eframe's web runner hands the app both the
//! key press (Ctrl+C, Ctrl+X, Ctrl+V) and the DOM clipboard event (`Copy`, `Cut`, `Paste`) in one
//! batch; egui-winit on the desktop swallows the key and sends only the clipboard event. Either
//! way each shortcut must act once, the same as the menu command.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

type H = egui_kittest::Harness<'static, SlideApp>;

fn app() -> H {
    app_with(Services::default())
}

fn app_with(services: Services) -> H {
    let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(1200.0, 800.0)).build_ui_state(
        |ui, app: &mut SlideApp| {
            app.logic(ui.ctx());
            app.ui(ui);
        },
        SlideApp::new(Session::with_new(), services),
    );
    h.run_steps(2);
    h
}

/// The batch a browser gives for Ctrl+`key`: the key press, the DOM clipboard event `ev`, and the
/// release. Returns the text the frame put on the system clipboard, if it put any.
pub(crate) fn web_shortcut(h: &mut H, key: egui::Key, ev: Option<egui::Event>) -> Option<String> {
    web_chord(h, key, CTRL, ev)
}

/// Ctrl on Windows and Linux; on a Mac egui reports Cmd as `mac_cmd` and `command`.
const CTRL: egui::Modifiers =
    egui::Modifiers { command: true, ctrl: !cfg!(target_os = "macos"), mac_cmd: cfg!(target_os = "macos"), ..egui::Modifiers::NONE };

/// The batch a browser gives for `key` with `mods` held: the key press, the DOM clipboard event
/// `ev`, and the release. Returns the text the frame put on the system clipboard, if any.
fn web_chord(h: &mut H, key: egui::Key, mods: egui::Modifiers, ev: Option<egui::Event>) -> Option<String> {
    let k = |pressed| egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: mods };
    h.input_mut().events.push(k(true));
    h.input_mut().events.extend(ev);
    h.input_mut().events.push(k(false));
    h.run_steps(1);
    let copied = h.output().platform_output.commands.iter().rev().find_map(|c| match c {
        egui::output::OutputCommand::CopyText(t) => Some(t.clone()),
        _ => None,
    });
    h.run_steps(1);
    copied
}

/// Undo, check, Redo: the last action was one Undo step.
fn one_undo_step(h: &mut H, before: impl Fn(&H) -> bool, what: &str) {
    h.state_mut().run("edit.undo", json!({})).unwrap();
    h.run_steps(1);
    assert!(before(h), "one Undo takes back {what}");
    h.state_mut().run("edit.redo", json!({})).unwrap();
    h.run_steps(1);
}

fn shapes(h: &H) -> usize {
    h.state().session.active().unwrap().current_slide().unwrap().shapes.len()
}

fn slides(h: &H) -> usize {
    h.state().session.active().unwrap().doc.slides.len()
}

fn rect(h: &mut H) -> u64 {
    let v = h.state_mut().run("shape.insert", json!({"preset": "rect", "rect": [80.0, 80.0, 120.0, 80.0]})).unwrap();
    h.state_mut().run("text.exit", json!({})).ok();
    let id = v["id"].as_u64().unwrap();
    h.state_mut().run("edit.select", json!({"ids": [id]})).unwrap();
    h.run_steps(1);
    id
}

#[test]
fn web_ctrl_c_ctrl_v_pastes_a_shape_once() {
    let mut h = app();
    rect(&mut h);
    let before = shapes(&h);
    let clip = web_shortcut(&mut h, egui::Key::C, Some(egui::Event::Copy)).expect("the copy reached the system clipboard");
    web_shortcut(&mut h, egui::Key::V, Some(egui::Event::Paste(clip)));
    assert_eq!(shapes(&h), before + 1, "one copy pasted");
    h.state_mut().run("edit.undo", json!({})).unwrap();
    assert_eq!(shapes(&h), before, "and one Undo takes it back");
}

#[test]
fn web_ctrl_x_cuts_the_shape_not_the_slide() {
    let mut h = app();
    h.state_mut().run("slide.new", json!({})).unwrap();
    let id = rect(&mut h);
    let (n_slides, before) = (slides(&h), shapes(&h));
    let clip = web_shortcut(&mut h, egui::Key::X, Some(egui::Event::Cut)).expect("the cut reached the system clipboard");
    assert_eq!(slides(&h), n_slides, "no slide cut");
    assert_eq!(shapes(&h), before - 1, "the shape is cut");
    let st = h.state().session.active().unwrap();
    assert!(st.current_slide().unwrap().shape(deckcraft_model::ShapeId(id as u32)).is_none());
    assert_eq!(clip, OBJECTS_MARKER, "a textless shape leaves the objects marker");
    one_undo_step(&mut h, |h| shapes(h) == before && slides(h) == n_slides, "the cut");
    web_shortcut(&mut h, egui::Key::V, Some(egui::Event::Paste(clip)));
    assert_eq!(shapes(&h), before, "the shape is pasted back once");
    assert_eq!(slides(&h), n_slides);
    one_undo_step(&mut h, |h| shapes(h) == before - 1, "the paste");
}

#[test]
fn menu_copy_then_web_ctrl_v_pastes_once() {
    let mut h = app();
    rect(&mut h);
    let before = shapes(&h);
    h.state_mut().run("edit.copy", json!({})).unwrap();
    h.run_steps(2);
    web_shortcut(&mut h, egui::Key::V, Some(egui::Event::Paste(OBJECTS_MARKER.into())));
    assert_eq!(shapes(&h), before + 1, "one copy pasted");
}

/// A press in the thumbnail pane selects the slide; Ctrl+C, Ctrl+V and Ctrl+X then act on it.
#[test]
fn web_shortcuts_copy_cut_and_paste_a_selected_slide() {
    let mut h = app();
    for _ in 0..2 {
        h.state_mut().run("slide.new", json!({})).unwrap();
    }
    h.state_mut().run("slide.go", json!({"index": 0})).unwrap();
    h.run_steps(3);
    let w = h.state().ui.thumbs_width;
    let pos = egui::pos2(w / 2.0, 300.0);
    h.event(egui::Event::PointerMoved(pos));
    for pressed in [true, false] {
        h.event(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE });
    }
    h.run_steps(2);
    assert!(h.state().slides_focused, "the thumbnail pane has the keyboard");
    assert!(!h.state().session.active().unwrap().selection.slides.is_empty(), "a slide is selected");
    let n = slides(&h);
    let clip = web_shortcut(&mut h, egui::Key::C, Some(egui::Event::Copy)).expect("the copy reached the system clipboard");
    web_shortcut(&mut h, egui::Key::V, Some(egui::Event::Paste(clip)));
    assert_eq!(slides(&h), n + 1, "one slide pasted");
    one_undo_step(&mut h, |h| slides(h) == n, "the slide paste");
    let clip = web_shortcut(&mut h, egui::Key::X, Some(egui::Event::Cut)).expect("the cut reached the system clipboard");
    assert_eq!(slides(&h), n, "the selected slide is cut");
    one_undo_step(&mut h, |h| slides(h) == n + 1, "the slide cut");
    web_shortcut(&mut h, egui::Key::V, Some(egui::Event::Paste(clip)));
    assert_eq!(slides(&h), n + 1, "and pasted back once");
    one_undo_step(&mut h, |h| slides(h) == n, "the second slide paste");
}

/// Text selected inside a shape: Ctrl+C then Ctrl+V at the end pastes the words once; Ctrl+X
/// takes them out.
#[test]
fn web_shortcuts_copy_cut_and_paste_text_in_a_shape_once() {
    let mut h = app();
    let v = h.state_mut().run("shape.insert", json!({"preset": "rect", "rect": [80.0, 80.0, 200.0, 80.0], "text": "Ship"})).unwrap();
    let id = v["id"].as_u64().unwrap();
    let text = |h: &H| h.state().session.active().unwrap().shape(deckcraft_model::ShapeId(id as u32)).unwrap().text.as_ref().unwrap().text();
    h.state_mut().run("text.edit", json!({"id": id})).unwrap();
    h.state_mut().run("text.selectAll", json!({})).unwrap();
    h.run_steps(1);
    let clip = web_shortcut(&mut h, egui::Key::C, Some(egui::Event::Copy)).expect("the copy reached the system clipboard");
    assert_eq!(clip, "Ship");
    h.state_mut().run("text.move", json!({"to": "end"})).unwrap();
    web_shortcut(&mut h, egui::Key::V, Some(egui::Event::Paste(clip)));
    assert_eq!(text(&h), "ShipShip", "pasted once");
    one_undo_step(&mut h, |h| text(h) == "Ship", "the text paste");
    h.state_mut().run("text.move", json!({"to": "start", "extend": true})).unwrap();
    web_shortcut(&mut h, egui::Key::X, Some(egui::Event::Cut));
    assert_eq!(text(&h), "", "cut once");
    one_undo_step(&mut h, |h| text(h) == "ShipShip", "the text cut");
}

/// A platform that sends only the key (no clipboard event) still copies and pastes.
#[test]
fn a_bare_shortcut_key_still_copies_and_pastes() {
    let mut h = app();
    rect(&mut h);
    let before = shapes(&h);
    web_shortcut(&mut h, egui::Key::C, None);
    web_shortcut(&mut h, egui::Key::V, None);
    assert_eq!(shapes(&h), before + 1, "one copy pasted");
}

/// Text box "Ship" in text editing, the caret at the end, with "Ship" on the session's clipboard.
fn ship_with_caret_at_end(h: &mut H) -> u64 {
    let v = h.state_mut().run("shape.insert", json!({"preset": "rect", "rect": [80.0, 80.0, 200.0, 80.0], "text": "Ship"})).unwrap();
    let id = v["id"].as_u64().unwrap();
    h.state_mut().run("text.edit", json!({"id": id})).unwrap();
    h.state_mut().run("text.selectAll", json!({})).unwrap();
    h.state_mut().run("edit.copy", json!({})).unwrap();
    h.state_mut().run("text.move", json!({"to": "end"})).unwrap();
    h.run_steps(1);
    id
}

fn shape_text(h: &H, id: u64) -> String {
    h.state().session.active().unwrap().shape(deckcraft_model::ShapeId(id as u32)).unwrap().text.as_ref().unwrap().text()
}

/// Whether the run holding `word` in shape `id` is bold.
fn bold_at(h: &H, id: u64, word: &str) -> bool {
    let st = h.state().session.active().unwrap();
    let body = st.shape(deckcraft_model::ShapeId(id as u32)).unwrap().text.clone().unwrap();
    body.paragraphs.iter().flat_map(|p| p.runs.iter()).filter(|r| r.text.contains(word)).any(|r| r.props.bold == Some(true))
}

/// The system clipboard holds "Other" in bold (its HTML).
fn bold_clipboard() -> Services {
    Services { clipboard_html: Some(Box::new(|| Some("<p><b>Other</b></p>".into()))), ..Default::default() }
}

/// DP1 fix 1 (I1): a browser sends Ctrl+Shift+V (Paste and Match Formatting) as the key and a DOM
/// paste with the system clipboard's text. It pastes that text once, as text (the clipboard's
/// bold is not kept, DC1), in one Undo step.
#[test]
fn web_ctrl_shift_v_pastes_the_clipboard_text_once_as_text() {
    let mut h = app_with(bold_clipboard());
    let id = ship_with_caret_at_end(&mut h);
    let chord = egui::Modifiers { shift: true, ..CTRL };
    web_chord(&mut h, egui::Key::V, chord, Some(egui::Event::Paste("Other".into())));
    assert_eq!(shape_text(&h, id), "ShipOther", "the browser's text, pasted once");
    assert!(!bold_at(&h, id, "Other"), "pasted as text: the clipboard's bold is not kept");
    one_undo_step(&mut h, |h| shape_text(h, id) == "Ship", "the paste");
}

/// The contrast: a plain Ctrl+V with the same clipboard keeps its bold, so the check above can
/// tell the two apart.
#[test]
fn web_ctrl_v_keeps_the_clipboard_formatting() {
    let mut h = app_with(bold_clipboard());
    let id = ship_with_caret_at_end(&mut h);
    web_chord(&mut h, egui::Key::V, CTRL, Some(egui::Event::Paste("Other".into())));
    assert_eq!(shape_text(&h, id), "ShipOther");
    assert!(bold_at(&h, id, "Other"), "a plain paste keeps the HTML's bold");
}

/// On the desktop Ctrl+Shift+V arrives as the key only: it pastes the copied text as text.
#[test]
fn a_bare_ctrl_shift_v_pastes_the_copied_text() {
    let mut h = app();
    let id = ship_with_caret_at_end(&mut h);
    web_chord(&mut h, egui::Key::V, egui::Modifiers { shift: true, ..CTRL }, None);
    assert_eq!(shape_text(&h, id), "ShipShip");
}

/// A browser may send a DOM paste for Ctrl+Alt+V too: Paste Special opens and nothing is pasted
/// until the person picks a format.
#[test]
fn web_ctrl_alt_v_opens_paste_special_and_pastes_nothing() {
    let mut h = app();
    rect(&mut h);
    let before = shapes(&h);
    h.state_mut().run("edit.copy", json!({})).unwrap();
    web_chord(&mut h, egui::Key::V, egui::Modifiers { alt: true, ..CTRL }, Some(egui::Event::Paste(OBJECTS_MARKER.into())));
    assert_eq!(shapes(&h), before, "nothing pasted");
    assert!(h.state().dialog.is_some(), "Paste Special is open");
}

/// The Windows clipboard keys: Shift+Delete cuts, Ctrl+Insert copies, Shift+Insert pastes. With
/// the DOM event in the batch, each acts once; Shift+Delete cuts the shape, not the slide.
#[test]
fn web_shift_delete_and_the_insert_keys_act_once() {
    let mut h = app();
    h.state_mut().run("slide.new", json!({})).unwrap();
    rect(&mut h);
    let (n_slides, before) = (slides(&h), shapes(&h));
    let shift = egui::Modifiers::SHIFT;
    let clip = web_chord(&mut h, egui::Key::Delete, shift, Some(egui::Event::Cut)).expect("the cut reached the system clipboard");
    assert_eq!((slides(&h), shapes(&h)), (n_slides, before - 1), "the shape is cut, not the slide");
    web_chord(&mut h, egui::Key::Insert, shift, Some(egui::Event::Paste(clip)));
    assert_eq!(shapes(&h), before, "pasted back once");
    let ctrl = egui::Modifiers { ctrl: true, command: true, ..Default::default() };
    let clip = web_chord(&mut h, egui::Key::Insert, ctrl, Some(egui::Event::Copy)).expect("the copy reached the system clipboard");
    web_chord(&mut h, egui::Key::Insert, shift, Some(egui::Event::Paste(clip)));
    assert_eq!((slides(&h), shapes(&h)), (n_slides, before + 1), "one copy pasted");
}

/// A Mac browser: Cmd (`mac_cmd`, `command`, no Ctrl) with the DOM events pastes once.
#[test]
fn web_cmd_c_cmd_v_on_a_mac_pastes_once() {
    let mut h = app();
    rect(&mut h);
    let before = shapes(&h);
    let cmd = egui::Modifiers::MAC_CMD | egui::Modifiers::COMMAND;
    let clip = web_chord(&mut h, egui::Key::C, cmd, Some(egui::Event::Copy)).expect("the copy reached the system clipboard");
    web_chord(&mut h, egui::Key::V, cmd, Some(egui::Event::Paste(clip)));
    assert_eq!(shapes(&h), before + 1, "one copy pasted");
}
